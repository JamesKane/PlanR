//! `touch [-c] [-t time] file ...`: sets each file's modification time
//! to now, or `time` (seconds since the epoch), creating it if it does
//! not exist, unless `-c`.

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

fn touch(name: &[u8], nocreate: bool, now: u32, tflag: bool) -> bool {
    let mut d = DirBuf::null();
    d.mtime = now;
    let Ok(c) = r::cstr(name) else {
        r::eprintln!("touch: {}: bad character in file name", Text(name));
        return false;
    };
    let Err(e) = r::dirwstat(&c, &d) else { return true };
    if nocreate {
        r::eprintln!("touch: {}: cannot wstat: {e}", Text(name));
        return false;
    }
    let mode = OpenMode {
        access: Access::Write,
        flags: OpenFlags::empty(),
    };
    let fd = match r::create(&c, mode, 0o666) {
        Ok(fd) => fd,
        Err(e) => {
            r::eprintln!("touch: {}: cannot create: {e}", Text(name));
            return false;
        }
    };
    let ok = !tflag
        || r::dirfwstat(fd, &d)
            .map_err(|e| r::eprintln!("touch: {}: cannot wstat: {e}", Text(name)))
            .is_ok();
    let _ = r::close(fd);
    ok
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a 32-bit time
    let mut now = r::time::time() as u32;
    let (mut nocreate, mut tflag) = (false, false);
    while let Some(c) = o.next() {
        match c {
            't' => {
                let Some(t) = o.arg() else { return usage() };
                tflag = true;
                #[allow(clippy::cast_possible_truncation)] // a 32-bit time
                {
                    now = r::num::strtoul(t, 0).0 as u32;
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
        ok &= touch(f, nocreate, now, tflag);
    }
    if ok { Ok(()) } else { Err("touch") }
}
