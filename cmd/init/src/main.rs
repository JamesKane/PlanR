//! The first user process. It builds the name space, as Plan 9's
//! `/lib/namespace` would, opens the console as descriptors 0, 1 and 2,
//! then runs the test programs from the boot archive at once, each with a
//! copy of its name space, environment and descriptors, and checks how
//! each one exits. It prints a line for each program (only failures with
//! `-q`) and a summary; it exits normally only if every program passed.

#![no_std]
#![no_main]

use core::ffi::CStr;
use core::fmt::Write as _;

use r::{Access, BindFlags, OpenFlags, OpenMode, RforkFlags as F};

r::main!(run);

const fn mode(access: Access) -> OpenMode {
    OpenMode {
        access,
        flags: OpenFlags::empty(),
    }
}

const READ: OpenMode = mode(Access::Read);
const WRITE: OpenMode = mode(Access::Write);

type Check = Result<(), &'static str>;

fn ensure(ok: bool, why: &'static str) -> Check {
    if ok { Ok(()) } else { Err(why) }
}

/// What a program should exit with: the status after its name and pid,
/// exactly or as a prefix.
#[derive(Clone, Copy)]
enum Want {
    Normal,
    Status(&'static [u8]),
    Trap,
}

const PROGRAMS: &[(&CStr, &[&CStr], Want)] = &[
    (c"/boot/tmem", &[c"tmem"], Want::Normal),
    (
        c"/boot/targs",
        &[c"targs", c"one", c"two"],
        Want::Status(b"targs one two"),
    ),
    (c"/boot/texec", &[c"texec"], Want::Status(b"targs from exec")),
    (c"/boot/tfault", &[c"tfault", c"null"], Want::Trap),
    (c"/boot/tfault", &[c"tfault", c"text"], Want::Trap),
    (c"/boot/tfault", &[c"tfault", c"brk"], Want::Trap),
    (c"/boot/tfault", &[c"tfault", c"stack"], Want::Trap),
    (c"/boot/tfile", &[c"tfile"], Want::Status(b"targs from env")),
    (c"/boot/tproc", &[c"tproc"], Want::Normal),
    (c"/boot/tnote", &[c"tnote"], Want::Normal),
    (c"/boot/t9p", &[c"t9p"], Want::Normal),
];

/// The binds, in order: what, where, how.
const NAMESPACE: &[(&CStr, &CStr, BindFlags)] = &[
    (c"#c", c"/dev", BindFlags::empty()),
    (c"#r", c"/dev", BindFlags::AFTER),
    (c"#P", c"/dev", BindFlags::AFTER),
    (c"#e", c"/env", BindFlags::CREATE),
    (c"#p", c"/proc", BindFlags::empty()),
    (c"#s", c"/srv", BindFlags::CREATE),
    (c"#d", c"/fd", BindFlags::empty()),
];

/// Writes `text` to standard output, which is the console.
fn say(text: &[u8]) {
    let _ = r::write(1, text);
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let quiet = args.get(1) == Some(b"-q");
    namespace()?;
    let mut pids = [0u64; PROGRAMS.len()];
    for (i, &(path, argv, _)) in PROGRAMS.iter().enumerate() {
        pids[i] = start(path, argv)?;
    }
    let mut passed = [false; PROGRAMS.len()];
    let mut text = [0u8; 256];
    let mut msg = [0u8; 256];
    for _ in 0..PROGRAMS.len() {
        let n = r::await_child(&mut text).map_err(|_| "await found no child")?;
        let (pid, _, m) = abi::parse_waitmsg(&text[..n], &mut msg).ok_or("bad wait message")?;
        let i = pids.iter().position(|&p| p == pid).ok_or("await gave a stranger")?;
        let msg = &msg[..m];
        passed[i] = exited_as(msg, pid, PROGRAMS[i].2);
        if !passed[i] || !quiet {
            let mut line = r::Buf::new();
            let _ = write!(line, "init: {}: ", Joined(PROGRAMS[i].1));
            line.put(if passed[i] { b"ok" } else { b"FAILED: " });
            if !passed[i] {
                line.put(msg);
            }
            line.put(b"\n");
            say(line.as_bytes());
        }
    }
    ensure(r::await_child(&mut text).is_err(), "await found a child too many")?;
    let failed = passed.iter().filter(|&&p| !p).count();
    let mut line = r::Buf::new();
    let _ = writeln!(line, "init: {} passed, {failed} failed", PROGRAMS.len() - failed);
    say(line.as_bytes());
    ensure(failed == 0, "test programs failed")
}

/// A program's arguments, shown as one line.
struct Joined<'a>(&'a [&'a CStr]);

impl core::fmt::Display for Joined<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, a) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            f.write_str(a.to_str().unwrap_or("?"))?;
        }
        Ok(())
    }
}

/// Builds the name space, opens the console as 0, 1 and 2, and checks the
/// binds took: the console and clock in `/dev`, this process in `/proc`,
/// and `/env` taking a new file.
fn namespace() -> Check {
    for &(new, old, flags) in NAMESPACE {
        r::bind(new, old, flags).map_err(|_| "a bind failed")?;
    }
    ensure(r::open(c"/dev/cons", READ) == Ok(0), "cannot open /dev/cons as 0")?;
    ensure(r::open(c"/dev/cons", WRITE) == Ok(1), "cannot open /dev/cons as 1")?;
    ensure(r::dup(1, None) == Ok(2), "cannot dup 1 to 2")?;
    let mut buf = [0u8; 64];
    let n = read_file(c"/dev/pid", &mut buf)?;
    let pid = core::str::from_utf8(&buf[..n])
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .ok_or("/dev/pid is not a number")?;
    let mut name = r::Buf::new();
    let _ = write!(name, "/proc/{pid}/status\0");
    let status = CStr::from_bytes_with_nul(name.as_bytes()).map_err(|_| "bad /proc name")?;
    ensure(read_file(status, &mut buf)? > 0, "this process's status is empty")?;
    ensure(read_file(c"/dev/rtc", &mut buf)? > 0, "/dev/rtc is empty")?;
    let fd = r::create(c"/env/init", WRITE, 0o664).map_err(|_| "cannot create in /env")?;
    ensure(r::write(fd, b"started") == Ok(7), "cannot write to /env")?;
    r::close(fd).map_err(|_| "close failed")?;
    r::remove(c"/env/init").map_err(|_| "cannot remove from /env")?;
    ensure(r::open(c"/fd/0ctl", READ).is_ok(), "/fd lacks 0ctl")
}

fn read_file(name: &CStr, buf: &mut [u8]) -> Result<usize, &'static str> {
    let fd = r::open(name, READ).map_err(|_| "cannot open a file to read it")?;
    let mut n = 0;
    while n < buf.len() {
        match r::read(fd, &mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => {
                let _ = r::close(fd);
                return Err("read failed");
            }
        }
    }
    r::close(fd).map_err(|_| "close failed")?;
    Ok(n)
}

/// Starts `path` in a child with copies of the name space, environment
/// and descriptors, and a note group of its own; returns its pid.
fn start(path: &CStr, argv: &[&CStr]) -> Result<u64, &'static str> {
    match r::rfork(F::PROC | F::NAMEG | F::ENVG | F::FDG | F::NOTEG) {
        Err(_) => Err("rfork failed"),
        Ok(0) => {
            let e = r::exec(path, argv);
            let mut why = r::Buf::new();
            why.put(b"init: exec failed: ");
            why.put(e.as_bytes());
            r::exits(Some(why.as_bytes()))
        }
        Ok(pid) => Ok(pid),
    }
}

/// Whether wait message text `msg` (already unquoted) is the exit `want`
/// asks of process `pid`: empty for a normal exit, else "name pid:
/// status".
fn exited_as(msg: &[u8], pid: u64, want: Want) -> bool {
    let mut prefix = r::Buf::new();
    let _ = write!(prefix, " {pid}: ");
    let p = prefix.as_bytes();
    let status = msg.windows(p.len()).position(|w| w == p).map(|at| &msg[at + p.len()..]);
    match (want, status) {
        (Want::Normal, _) => msg.is_empty(),
        (Want::Status(s), Some(st)) => st == s,
        (Want::Trap, Some(st)) => st.starts_with(b"sys: trap: "),
        _ => false,
    }
}
