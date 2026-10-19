//! libr's system layer, in a real process: directory entries, the
//! environment, `exec` through `$path`, `wait`, `postnote`, the pid and
//! the time. `/sys/test/run` runs it in the name space `/lib/namespace`
//! builds (`/dev`, `/env`, `/proc`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use r::{RforkFlags as F, Whom};

r::main!(run);

type Check = Result<(), &'static str>;

fn ensure(ok: bool, why: &'static str) -> Check {
    if ok { Ok(()) } else { Err(why) }
}

fn run(_: r::Args) -> Result<(), &'static str> {
    dirs()?;
    env()?;
    exec_path()?;
    notes()?;
    time()
}

fn dirs() -> Check {
    let d = r::dirstat(c"/dev/cons").map_err(|_| "dirstat failed")?;
    ensure(
        d.name == "cons" && !d.is_dir() && d.mode & 0o777 == 0o660,
        "dirstat of /dev/cons",
    )?;
    let root = r::dirstat(c"/").map_err(|_| "dirstat / failed")?;
    ensure(root.is_dir(), "/ is not a directory")?;
    let fd = r::open(c"/dev", READ).map_err(|_| "cannot open /dev")?;
    let all = r::dirreadall(fd).map_err(|_| "dirreadall failed")?;
    let f = r::dirfstat(fd).map_err(|_| "dirfstat failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    ensure(f.is_dir(), "dirfstat of /dev")?;
    // `#c` and `#P` are both bound there.
    for want in ["cons", "consctl", "pid", "cputype"] {
        ensure(all.iter().any(|d| d.name == want), "an entry missing from /dev")?;
    }
    // The devices here take no wstat; the error is theirs, not ours.
    let mut n = r::DirBuf::null();
    n.name = "renamed".into();
    let e = r::dirwstat(c"/dev/null", &n).err().ok_or("wstat of /dev/null worked")?;
    ensure(e.as_bytes() == b"permission denied", "dirwstat's error")
}

const READ: r::OpenMode = r::OpenMode {
    access: r::Access::Read,
    flags: r::OpenFlags::empty(),
};

fn env() -> Check {
    r::putenv(b"tsys", b"one value").map_err(|_| "putenv failed")?;
    ensure(r::getenv(b"tsys").as_deref() == Some(&b"one value"[..]), "getenv")?;
    r::putenv_list(b"tsysl", &[b"a", b"b c", b""]).map_err(|_| "putenv_list failed")?;
    ensure(
        r::getenv(b"tsysl").as_deref() == Some(&b"a b c "[..]),
        "getenv of a list",
    )?;
    let l = r::getenv_list(b"tsysl").ok_or("getenv_list failed")?;
    ensure(l == [&b"a"[..], b"b c", b""], "getenv_list")?;
    ensure(r::getenv(b"no such variable").is_none(), "getenv of nothing")?;
    ensure(r::putenv(b"a/b", b"x").is_err(), "putenv of a bad name")?;
    r::remove(c"/env/tsys").map_err(|_| "remove failed")?;
    r::remove(c"/env/tsysl").map_err(|_| "remove failed")
}

/// Runs `name` through `$path` in a child; returns its exit status.
fn run_child(name: &[u8], argv: &[&core::ffi::CStr]) -> Result<Vec<u8>, &'static str> {
    match r::rfork(F::PROC | F::FDG) {
        Err(_) => Err("rfork failed"),
        Ok(0) => {
            let e = r::exec_path(name, argv);
            r::exits(Some(e.as_bytes()))
        }
        Ok(pid) => {
            let w = r::wait().map_err(|_| "wait failed")?;
            ensure(w.pid == pid, "wait gave another pid")?;
            Ok(w.status().to_vec())
        }
    }
}

fn exec_path() -> Check {
    r::putenv_list(b"path", &[b"/nowhere", b"/bin"]).map_err(|_| "putenv path failed")?;
    let s = run_child(b"targs", &[c"targs", c"via", c"path"])?;
    ensure(s == b"targs via path", "targs through $path")?;
    let s = run_child(b"/bin/targs", &[c"targs", c"rooted"])?;
    ensure(s == b"targs rooted", "targs by its full name")?;
    let s = run_child(b"nonesuch", &[c"nonesuch"])?;
    ensure(s == b"file does not exist", "a command that is nowhere")?;
    r::remove(c"/env/path").map_err(|_| "remove failed")
}

fn notes() -> Check {
    let me = r::getpid().map_err(|_| "getpid failed")?;
    let mut buf = [0u8; 32];
    let fd = r::open(c"/dev/pid", READ).map_err(|_| "cannot open /dev/pid")?;
    let n = r::read(fd, &mut buf).map_err(|_| "read failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    let want: u64 = core::str::from_utf8(&buf[..n])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or("/dev/pid")?;
    ensure(me == want, "getpid")?;

    // A note to one process, then one to the group (which spares us).
    for whom in [Whom::Proc, Whom::Group] {
        let pid = match r::rfork(F::PROC) {
            Err(_) => return Err("rfork failed"),
            Ok(0) => {
                let _ = r::sleep(10_000);
                r::exits(Some(b"not noted"))
            }
            Ok(pid) => pid,
        };
        // Let it get to its sleep; a note before then ends it as well.
        let _ = r::sleep(20);
        let target = if whom == Whom::Proc { pid } else { me };
        r::postnote(whom, target, b"go away").map_err(|_| "postnote failed")?;
        let w = r::wait().map_err(|_| "wait failed")?;
        ensure(
            w.pid == pid && w.status() == b"go away",
            "the note did not end the child",
        )?;
    }
    ensure(r::postnote(Whom::Proc, 999_999, b"x").is_err(), "a note to nobody")
}

fn time() -> Check {
    let t = r::time::time();
    // The clock starts from the firmware's, so it is past 2025.
    ensure(t > 1_735_689_600, "the time is before 2025")?;
    let s = r::time::ctime(t);
    ensure(
        s.len() == 29 && s.ends_with('\n') && s[20..23] == *"GMT",
        "ctime with no zone",
    )?;
    r::putenv(b"timezone", b"EST -18000 EDT -14400\n").map_err(|_| "putenv failed")?;
    let tm = r::time::localtime(t);
    r::remove(c"/env/timezone").map_err(|_| "remove failed")?;
    ensure(tm.zone == "EST" && tm.tzoff == -18000, "the zone from /env/timezone")?;
    ensure(r::time::tm2sec(&tm) == t, "tm2sec of localtime")
}
