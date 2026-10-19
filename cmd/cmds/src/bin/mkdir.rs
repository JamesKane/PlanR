//! `mkdir [-p] [-m mode] dir ...`: makes each directory, with
//! permissions `mode` (octal; 0777); with `-p`, and the directories on
//! the way to it that do not exist.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use r::{Access, OpenFlags, OpenMode, Text};

r::main!(run);

fn usage() -> Result<(), &'static str> {
    r::eprintln!("usage: mkdir [-p] [-m mode] dir...");
    Err("usage")
}

fn exists(s: &[u8]) -> bool {
    r::cstr(s).is_ok_and(|c| r::access(&c, r::AEXIST).is_ok())
}

/// Makes directory `s`; false (having said why) if it cannot.
fn makedir(s: &[u8], mode: u32) -> bool {
    if exists(s) {
        r::eprintln!("mkdir: {} already exists", Text(s));
        return false;
    }
    let open = OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    };
    match r::cstr(s).and_then(|c| r::create(&c, open, abi::DMDIR | mode)) {
        Ok(fd) => {
            let _ = r::close(fd);
            true
        }
        Err(e) => {
            r::eprintln!("mkdir: can't create {}: {e}", Text(s));
            false
        }
    }
}

/// Makes `s` and the directories above it that are missing.
fn mkdirp(s: &[u8], mode: u32) -> bool {
    for (i, _) in s.iter().enumerate().skip(1).filter(|&(_, &b)| b == b'/') {
        if !exists(&s[..i]) && !makedir(&s[..i], mode) {
            return false;
        }
    }
    exists(s) || makedir(s, mode)
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let (mut pflag, mut mode) = (false, 0o777);
    while let Some(c) = o.next() {
        match c {
            'm' => {
                let Some(m) = o.arg() else { return usage() };
                mode = r::num::strtoul(m, 8).0;
                if mode > 0o777 {
                    return usage();
                }
            }
            'p' => pflag = true,
            _ => return usage(),
        }
    }
    #[allow(clippy::cast_possible_truncation)] // at most 0777
    let mode = mode as u32;
    let mut ok = true;
    for d in o.rest() {
        ok &= if pflag { mkdirp(d, mode) } else { makedir(d, mode) };
    }
    if ok { Ok(()) } else { Err("error") }
}
