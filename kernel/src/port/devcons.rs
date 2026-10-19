//! `#c`, the console device: the console itself, and small files about the
//! process and the system, as Plan 9's `devcons` has them. Numbers read as
//! Plan 9 writes them: right-aligned in 11 columns, then a space.
//!
//! The console has no input yet, so reading `cons` fails.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use abi::{DMDIR, OpenMode, QTDIR, QTFILE, Qid};

use crate::port::chan::Chan;
use crate::port::dev::{self, Dev, EVE, Entry};
use crate::port::error::{EBADSPEC, ENONEXIST, EPERM, Result};
use crate::port::lock::SpinLock;
use crate::port::{clock, devrtc, proc};

const QDIR: u64 = 0;

/// The files: name, qid path and permissions.
const FILES: &[(&str, u64, u32)] = &[
    ("cons", 1, 0o660),
    ("drivers", 2, 0o444),
    ("null", 3, 0o666),
    ("osversion", 4, 0o444),
    ("pid", 5, 0o444),
    ("ppid", 6, 0o444),
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
        dev::open_checked(c, mode, p)
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        match c.qid.path {
            QDIR => dev::dirread_gen(c, buf, offset, &dirgen),
            QCONS => Err("console has no input yet".into()),
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
                crate::port::console::print(format_args!("{}", String::from_utf8_lossy(buf)));
                Ok(buf.len())
            }
            QNULL => Ok(buf.len()),
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
