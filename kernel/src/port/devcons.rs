//! `#c`, the console device: the console itself, and small files about the
//! process and the system, as Plan 9's `devcons` has them. Numbers read as
//! Plan 9 writes them: right-aligned in 11 columns, then a space.
//!
//! Input comes from the console UART's interrupt, which only queues the
//! bytes; the `kbd` process edits them into lines as Plan 9 does (`\r` is
//! `\n`, backspace erases a character, ^U the line, ^D sends the line
//! without a newline, and so ends input if it is empty) and echoes them.
//! DEL posts `interrupt` to the note group of the process that last read
//! `cons`. Writing `rawon` to `consctl` passes bytes through untouched and
//! unechoed until `rawoff`, or until the last `consctl` is closed. With no
//! known interrupt there is no input, and reading `cons` fails.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use core::sync::atomic::{AtomicBool, Ordering};

use abi::{DMDIR, OpenMode, QTDIR, QTFILE, Qid};

use crate::port::chan::Chan;
use crate::port::dev::{self, Dev, EVE, Entry};
use crate::port::error::{EBADCTL, EBADSPEC, ENONEXIST, EPERM, Result};
use crate::port::lock::SpinLock;
use crate::port::proc::{Rendez, Sleepable, Spawn};
use crate::port::qio::{Block, Queue};
use crate::port::{clock, console, devrtc, note, proc};

const QDIR: u64 = 0;

/// The files: name, qid path and permissions.
const FILES: &[(&str, u64, u32)] = &[
    ("cons", 1, 0o660),
    ("consctl", 11, 0o220),
    ("drivers", 2, 0o444),
    ("null", 3, 0o666),
    ("osversion", 4, 0o444),
    ("pid", 5, 0o444),
    ("ppid", 6, 0o444),
    ("reboot", 12, 0o660),
    ("sysname", 7, 0o664),
    ("time", 8, 0o444),
    ("user", 9, 0o444),
    ("zero", 10, 0o444),
];
const QCONS: u64 = 1;
const QDRIVERS: u64 = 2;
const QNULL: u64 = 3;
const QOSVERSION: u64 = 4;
const QPID: u64 = 5;
const QPPID: u64 = 6;
const QSYSNAME: u64 = 7;
const QTIME: u64 = 8;
const QUSER: u64 = 9;
const QZERO: u64 = 10;
const QCONSCTL: u64 = 11;
const QREBOOT: u64 = 12;

/// The longest `sysname`.
const SYSNAME_MAX: usize = 64;

static SYSNAME_TEXT: SpinLock<String> = SpinLock::new(String::new());

pub(crate) struct Cons;

pub(crate) static CONS: Cons = Cons;

fn qid(path: u64) -> Qid {
    Qid {
        path,
        vers: 0,
        ty: if path == QDIR { QTDIR } else { QTFILE },
    }
}

fn dirgen(_: &Chan, i: usize) -> Option<Entry> {
    let &(name, path, perm) = FILES.get(i)?;
    Some(Entry::new(name, qid(path), 0, perm))
}

fn perm(path: u64) -> u32 {
    if path == QDIR {
        return DMDIR | 0o555;
    }
    FILES.iter().find(|f| f.1 == path).map_or(0, |f| f.2)
}

/// A number as Plan 9's `readnum` gives it.
fn num(n: u64) -> String {
    alloc::format!("{n:>11} ")
}

/// What reading file `path` gives, for the files that are text.
fn text(path: u64) -> Option<String> {
    let me = proc::current();
    Some(match path {
        QDRIVERS => {
            let mut s = String::new();
            for d in dev::all() {
                let _ = writeln!(s, "#{} {}", d.dc(), d.name());
            }
            s
        }
        QOSVERSION => alloc::format!("{}\n", env!("CARGO_PKG_VERSION")),
        QPID => num(me.map_or(0, |p| p.pid())),
        QPPID => num(me.map_or(0, |p| p.ppid())),
        QSYSNAME => match SYSNAME_TEXT.lock().clone() {
            s if s.is_empty() => String::from("planr"),
            s => s,
        },
        // Seconds, nanoseconds since the epoch, counter ticks, and their
        // frequency.
        QTIME => {
            let ns = devrtc::nanoseconds();
            alloc::format!(
                "{:>11} {:>20} {:>20} {:>20} ",
                ns / 1_000_000_000,
                ns,
                crate::arch::counter(),
                clock::counter_hz()
            )
        }
        QUSER => String::from(EVE),
        _ => return None,
    })
}

impl Dev for Cons {
    fn dc(&self) -> char {
        'c'
    }

    fn name(&self) -> &'static str {
        "cons"
    }

    fn attach(&self, spec: &str) -> Result<Chan> {
        if !spec.is_empty() {
            return Err(EBADSPEC.into());
        }
        Ok(Chan::new(&CONS, 0, qid(QDIR), String::from("#c")))
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Chan> {
        dev::walk_gen(c, name, &dirgen)
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        let e = match c.qid.path {
            QDIR => Entry::new("#c", c.qid, 0, perm(QDIR)),
            p => {
                let f = FILES.iter().find(|f| f.1 == p).ok_or(ENONEXIST)?;
                Entry::new(f.0, c.qid, 0, f.2)
            }
        };
        Ok(e.encode(self, c.devno))
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        let p = perm(c.qid.path);
        let c = dev::open_checked(c, mode, p)?;
        if c.qid.path == QCONSCTL {
            KBD.lock().ctl += 1;
        }
        Ok(c)
    }

    fn close(&self, c: &Chan) {
        if c.qid.path == QCONSCTL {
            let mut k = KBD.lock();
            k.ctl -= 1;
            if k.ctl == 0 {
                k.raw = false;
            }
        }
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        match c.qid.path {
            QDIR => dev::dirread_gen(c, buf, offset, &dirgen),
            QCONS => read_cons(buf),
            QNULL => Ok(0),
            QZERO => {
                buf.fill(0);
                Ok(buf.len())
            }
            p => Ok(dev::read_bytes(text(p).ok_or(ENONEXIST)?.as_bytes(), buf, offset)),
        }
    }

    fn write(&self, c: &Chan, buf: &[u8], _: u64) -> Result<usize> {
        match c.qid.path {
            QCONS => {
                console::write_bytes(buf);
                Ok(buf.len())
            }
            QCONSCTL => {
                consctl(buf)?;
                Ok(buf.len())
            }
            QNULL => Ok(buf.len()),
            QREBOOT => reboot(buf),
            QSYSNAME => {
                let s = core::str::from_utf8(buf).map_err(|_| EPERM)?;
                let s = s.trim_end_matches('\n');
                let cut = s.char_indices().nth(SYSNAME_MAX).map_or(s.len(), |(i, _)| i);
                *SYSNAME_TEXT.lock() = String::from(&s[..cut]);
                Ok(buf.len())
            }
            _ => Err(EPERM.into()),
        }
    }
}

/// `reboot`, as Plan 9's: `halt` stops the machine (in QEMU, it exits),
/// `reboot` with no file resets it, `panic` panics.
fn reboot(buf: &[u8]) -> Result<usize> {
    let text = core::str::from_utf8(buf).map_err(|_| EBADCTL)?;
    let words: alloc::vec::Vec<&str> = text.split_ascii_whitespace().collect();
    match words.as_slice() {
        ["halt"] => {
            crate::println!("kernel: halt");
            crate::arch::exit(crate::arch::ExitCode::Success)
        }
        ["reboot"] => crate::arch::reset(None),
        ["reboot", ..] => Err("reboot into another kernel is not yet supported".into()),
        ["panic", ..] => panic!("/dev/reboot"),
        _ => Err(EBADCTL.into()),
    }
}

/// Bytes the interrupt has taken from the UART, for `kbd`.
struct Raw {
    buf: [u8; RAW_MAX],
    start: usize,
    len: usize,
}

const RAW_MAX: usize = 1024;

static RAW: SpinLock<Raw> = SpinLock::new(Raw {
    buf: [0; RAW_MAX],
    start: 0,
    len: 0,
});
/// Where `kbd` waits for bytes.
static RAW_READY: Rendez = Rendez::new();

/// The line discipline's state.
#[derive(Debug)]
struct Kbd {
    raw: bool,
    /// The line being typed.
    line: Vec<u8>,
    /// The note group of the last reader of `cons`; 0 for none yet.
    noteid: u64,
    /// Open `consctl` chans.
    ctl: usize,
}

static KBD: SpinLock<Kbd> = SpinLock::new(Kbd {
    raw: false,
    line: Vec::new(),
    noteid: 0,
    ctl: 0,
});

/// What reading `cons` gives: one line, or what raw input there was, per
/// block.
static INPUT: Queue = Queue::new(INPUT_MAX, false);
/// Input beyond this is dropped while nothing reads it.
const INPUT_MAX: usize = 64 * 1024;
/// The longest line: it is sent as it is once this long, as Plan 9 does.
const LINE_MAX: usize = 1024;

static HAVE_INPUT: AtomicBool = AtomicBool::new(false);

const ERASE: u8 = 0x08;
const KILL: u8 = 0x15; // ^U
const EOT: u8 = 0x04; // ^D
const DEL: u8 = 0x7f;

/// Starts console input, if the UART's interrupt is known: the `kbd`
/// process, then the interrupt.
pub(crate) fn start_input() {
    let Some(irq) = console::input_irq() else {
        crate::println!("cons: no input (the console's interrupt is unknown)");
        return;
    };
    if proc::spawn(Spawn::kproc("kbd"), kbd).is_none() {
        crate::println!("cons: no input (no memory for kbd)");
        return;
    }
    match crate::arch::enable_irq(irq, "cons", uart_interrupt) {
        Ok(v) => {
            console::enable_input();
            HAVE_INPUT.store(true, Ordering::Release);
            crate::println!("cons: input on {irq:?}, vector {v:#x}");
        }
        Err(e) => crate::println!("cons: no input ({e})"),
    }
}

/// The UART's interrupt: takes every byte it has.
fn uart_interrupt() {
    let mut got = false;
    while let Some(b) = console::getc() {
        let mut r = RAW.lock();
        if r.len < RAW_MAX {
            let at = (r.start + r.len) % RAW_MAX;
            r.buf[at] = b;
            r.len += 1;
        }
        got = true;
    }
    if got {
        RAW_READY.wakeup();
    }
}

/// The `kbd` process: edits what the interrupt took.
fn kbd() {
    let s = proc::sleepable().expect("a kernel process may sleep");
    let mut bytes = [0u8; RAW_MAX];
    loop {
        RAW_READY.sleep(&s, || RAW.lock().len > 0);
        let n = {
            let mut r = RAW.lock();
            let n = r.len;
            for (i, b) in bytes[..n].iter_mut().enumerate() {
                *b = r.buf[(r.start + i) % RAW_MAX];
            }
            r.start = (r.start + n) % RAW_MAX;
            r.len = 0;
            n
        };
        let mut out = Edited::default();
        {
            let mut k = KBD.lock();
            for &b in &bytes[..n] {
                k.put(b, &mut out);
            }
            if !out.raw.is_empty() {
                out.send.push(core::mem::take(&mut out.raw));
            }
        }
        if !out.echo.is_empty() {
            console::write_bytes(&out.echo);
        }
        for v in out.send {
            send(&s, v);
        }
        if let Some(id) = out.interrupt {
            note::post_group(id, b"interrupt");
        }
    }
}

/// What a run of input bytes comes to.
#[derive(Default)]
struct Edited {
    echo: Vec<u8>,
    /// Finished lines, in order.
    send: Vec<Vec<u8>>,
    /// Raw bytes, sent as one block at the end.
    raw: Vec<u8>,
    /// A note group to interrupt.
    interrupt: Option<u64>,
}

impl Kbd {
    fn put(&mut self, b: u8, out: &mut Edited) {
        if self.raw {
            out.raw.push(b);
            return;
        }
        match b {
            DEL => {
                self.line.clear();
                if self.noteid != 0 {
                    out.interrupt = Some(self.noteid);
                }
            }
            ERASE => {
                if self.erase() {
                    out.echo.extend_from_slice(b"\x08 \x08");
                }
            }
            KILL => {
                while self.erase() {
                    out.echo.extend_from_slice(b"\x08 \x08");
                }
            }
            EOT => out.send.push(core::mem::take(&mut self.line)),
            b'\r' | b'\n' => {
                self.line.push(b'\n');
                out.echo.push(b'\n');
                out.send.push(core::mem::take(&mut self.line));
            }
            b => {
                self.line.push(b);
                out.echo.push(b);
                if self.line.len() >= LINE_MAX {
                    out.send.push(core::mem::take(&mut self.line));
                }
            }
        }
    }

    /// Takes the last character (a whole UTF-8 sequence) off the line;
    /// false if it was empty.
    fn erase(&mut self) -> bool {
        let Some(at) = self.line.iter().rposition(|&b| b & 0xc0 != 0x80) else {
            let had = !self.line.is_empty();
            self.line.clear();
            return had;
        };
        self.line.truncate(at);
        true
    }
}

/// Queues `v` for readers, unless they have let too much pile up.
fn send(s: &Sleepable, v: Vec<u8>) {
    if INPUT.len() < INPUT_MAX {
        let _ = INPUT.bwrite(s, Block::new(v));
    }
}

fn read_cons(buf: &mut [u8]) -> Result<usize> {
    if !HAVE_INPUT.load(Ordering::Acquire) {
        return Err("console has no input".into());
    }
    let s = proc::sleepable().ok_or(EPERM)?;
    if let Some(p) = proc::current() {
        KBD.lock().noteid = p.noteid();
    }
    INPUT.read(&s, buf)
}

/// A `consctl` request: `rawon` or `rawoff`, several to a write.
fn consctl(buf: &[u8]) -> Result<()> {
    let text = core::str::from_utf8(buf).map_err(|_| EBADCTL)?;
    for word in text.split_ascii_whitespace() {
        let flushed = {
            let mut k = KBD.lock();
            match word {
                // A line half typed goes to the reader as it is.
                "rawon" => {
                    k.raw = true;
                    core::mem::take(&mut k.line)
                }
                "rawoff" => {
                    k.raw = false;
                    Vec::new()
                }
                _ => return Err(EBADCTL.into()),
            }
        };
        if !flushed.is_empty() {
            send(&proc::sleepable().ok_or(EPERM)?, flushed);
        }
    }
    Ok(())
}
