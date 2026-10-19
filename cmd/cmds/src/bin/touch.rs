//! `touch [-c] [-t time] file ...`: sets each file's modification time
//! to now, or `time` (seconds since the epoch, in C's bases), creating it
//! if it does not exist, unless `-c` (Plan 9's `touch.c`).

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use r::{Access, DirBuf, OpenFlags, OpenMode, Text};

r::main!(run);

fn usage() -> Result<(), &'static str> {
    r::eprintln!("usage: touch [-c] [-t time] files");
    Err("usage")
}

/// Touches `name`; false (having said why) if it cannot.
fn touch(name: &[u8], nocreate: bool, now: u32) -> bool {
    let mut d = DirBuf::null();
    d.mtime = now;
    let c = match r::cstr(name) {
        Ok(c) => c,
        Err(e) => {
            r::eprintln!("touch: {}: cannot create: {e}", Text(name));
            return false;
        }
    };
    let Err(e) = r::dirwstat(&c, &d) else { return true };
    if nocreate {
        r::eprintln!("touch: {}: cannot wstat: {e}", Text(name));
        return false;
    }
    let mode = OpenMode {
        access: Access::Read,
        flags: OpenFlags::EXCL,
    };
    match r::create(&c, mode, 0o666) {
        Ok(fd) => {
            let _ = r::dirfwstat(fd, &d);
            let _ = r::close(fd);
            true
        }
        Err(e) => {
            r::eprintln!("touch: {}: cannot create: {e}", Text(name));
            false
        }
    }
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a 32-bit time
    let mut now = r::time::time() as u32;
    let mut nocreate = false;
    while let Some(c) = o.next() {
        match c {
            't' => {
                let Some(t) = o.arg() else { return usage() };
                let (v, n) = r::num::strtoul(t, 0);
                if n == 0 || n != t.len() {
                    return usage();
                }
                #[allow(clippy::cast_possible_truncation)] // a 32-bit time
                {
                    now = v as u32;
                }
            }
            'c' => nocreate = true,
            _ => return usage(),
        }
    }
    let files = o.rest();
    if files.is_empty() {
        return usage();
    }
    let mut ok = true;
    for f in files {
        ok &= touch(f, nocreate, now);
    }
    if ok { Ok(()) } else { Err("touch") }
}
