//! The Plan R half of `cargo xtask oracle`: runs each script of each
//! corpus in `/sys/lib/oracle` (the tree's `oracle/rc`, `oracle/cmd`, in
//! the boot archive) as `rc NAME.rc`, in a directory of its own in `/tmp`
//! with nothing else in it, standard input from `/dev/null`, and a copy
//! of this environment. Then it prints on the console what the script
//! wrote to standard output and error, and how it exited, for `xtask` to
//! compare with plan9port's; a script is named `CORPUS/NAME`:
//!
//! ```text
//! oracle: begin NAME
//! oracle: out HEX       (as many as it takes, 48 bytes each)
//! oracle: err HEX
//! oracle: status HEX    (after "rc pid: "; none for a normal exit)
//! oracle: end NAME
//! oracle: done N
//! ```
//!
//! It exits normally if every script ran, whatever the scripts did.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use abi::DMDIR;
use r::{Access, OpenFlags, OpenMode, RforkFlags as F};

r::main!(run);

type Check = Result<(), &'static str>;

const fn mode(access: Access) -> OpenMode {
    OpenMode {
        access,
        flags: OpenFlags::empty(),
    }
}

const READ: OpenMode = mode(Access::Read);
const WRITE: OpenMode = mode(Access::Write);

/// Where the corpora are, and where their scripts run.
const SCRIPTS: &str = "/sys/lib/oracle";
const WORK: &str = "/tmp/oracle";

/// Bytes of output on each line.
const CHUNK: usize = 48;

/// The entries of directory `name`.
fn entries(name: &str) -> Result<Vec<r::DirBuf>, &'static str> {
    let fd = r::open(&cstr(name), READ).map_err(|_| "no oracle scripts")?;
    let all = r::dir::dirreadall(fd).map_err(|_| "cannot read the oracle scripts")?;
    r::close(fd).map_err(|_| "close failed")?;
    Ok(all)
}

fn run(_: r::Args) -> Check {
    mkdir(WORK)?;
    // The scripts' names, `corpus/name` without `.rc`.
    let mut stems: Vec<String> = Vec::new();
    for corpus in entries(SCRIPTS)?.into_iter().filter(r::DirBuf::is_dir) {
        let c = &corpus.name;
        mkdir(&format!("{WORK}/{c}"))?;
        stems.extend(
            entries(&format!("{SCRIPTS}/{c}"))?
                .into_iter()
                .filter(|d| !d.is_dir())
                .filter_map(|d| d.name.strip_suffix(".rc").map(|n| format!("{c}/{n}"))),
        );
    }
    stems.sort();
    for stem in &stems {
        say(&format!("oracle: begin {stem}\n"));
        let ran = script(stem)?;
        hex("out", &ran.out);
        hex("err", &ran.err);
        if !ran.status.is_empty() {
            hex("status", &ran.status);
        }
        say(&format!("oracle: end {stem}\n"));
    }
    say(&format!("oracle: done {}\n", stems.len()));
    Ok(())
}

/// What a script did.
struct Ran {
    out: Vec<u8>,
    err: Vec<u8>,
    /// The exit status, after `rc pid: `.
    status: Vec<u8>,
}

/// Runs script `stem.rc`, `corpus/name`.
fn script(stem: &str) -> Result<Ran, &'static str> {
    let name = format!("{}.rc", stem.rsplit('/').next().unwrap_or(stem));
    let dir = format!("{WORK}/{stem}");
    mkdir(&dir)?;
    let text = r::read_all(&cstr(&format!("{SCRIPTS}/{stem}.rc"))).map_err(|_| "cannot read a script")?;
    put(&format!("{dir}/{name}"), &text)?;
    let (out, err) = (format!("{WORK}/{stem}.out"), format!("{WORK}/{stem}.err"));
    let pid = match r::rfork(F::PROC | F::FDG | F::ENVG | F::NOTEG) {
        Err(_) => return Err("rfork failed"),
        Ok(0) => {
            let ok = r::chdir(&cstr(&dir)).is_ok()
                && redirect(0, c"/dev/null", false)
                && redirect(1, &cstr(&out), true)
                && redirect(2, &cstr(&err), true);
            if !ok {
                r::exits(Some(b"toracle: cannot set up the script"));
            }
            let arg = cstr(&name);
            let e = r::exec(c"/boot/rc", &[c"rc", &arg]);
            r::exits(Some(e.as_bytes()))
        }
        Ok(pid) => pid,
    };
    let w = r::wait().map_err(|_| "wait failed")?;
    if w.pid != pid {
        return Err("wait gave a stranger");
    }
    let read = |f: &str| r::read_all(&cstr(f)).map_err(|_| "cannot read a script's output");
    Ok(Ran {
        out: read(&out)?,
        err: read(&err)?,
        status: w.status().to_vec(),
    })
}

/// Makes `fd` the file `name`, created if `create`.
fn redirect(fd: r::Fd, name: &core::ffi::CStr, create: bool) -> bool {
    let new = if create {
        r::create(name, WRITE, 0o664)
    } else {
        r::open(name, READ)
    };
    let Ok(new) = new else { return false };
    new == fd || (r::dup(new, Some(fd)).is_ok() && r::close(new).is_ok())
}

fn mkdir(name: &str) -> Check {
    let fd = r::create(&cstr(name), READ, DMDIR | 0o775).map_err(|_| "mkdir failed")?;
    r::close(fd).map_err(|_| "close failed")
}

fn put(name: &str, data: &[u8]) -> Check {
    let fd = r::create(&cstr(name), WRITE, 0o664).map_err(|_| "create failed")?;
    let ok = r::write(fd, data) == Ok(data.len());
    r::close(fd).map_err(|_| "close failed")?;
    if ok { Ok(()) } else { Err("write failed") }
}

/// Prints `data` as `oracle: what HEX` lines.
fn hex(what: &str, data: &[u8]) {
    for chunk in data.chunks(CHUNK) {
        let mut line = format!("oracle: {what} ");
        for b in chunk {
            let _ = write!(line, "{b:02x}");
        }
        line.push('\n');
        say(&line);
    }
}

/// Writes `text` to the console in one write, so that it is not split.
fn say(text: &str) {
    let _ = r::write(1, text.as_bytes());
}

fn cstr(s: &str) -> CString {
    r::cstr(s.as_bytes()).expect("no NUL")
}
