//! `bind [-q] [-b|-a|-c|-bc|-ac] new old`: binds `new` onto `old`, as
//! Plan 9's `bind`: in place of it, or before (`-b`) or after (`-a`) it
//! in a union; `-c` allows creating there. `-q` keeps quiet about a
//! failure, and exits normally.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

use r::{AEXIST, BindFlags, Rerr, Text};

r::main!(run);

fn usage() -> Result<(), &'static str> {
    r::eprintln!("usage: bind [-q] [-b|-a|-c|-bc|-ac] new old");
    Err("usage")
}

fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(cmdline.iter());
    let mut flags = BindFlags::empty();
    let mut quiet = false;
    while let Some(c) = o.next() {
        match c {
            'a' => flags = flags | BindFlags::AFTER,
            'b' => flags = flags | BindFlags::BEFORE,
            'c' => flags = flags | BindFlags::CREATE,
            'q' => quiet = true,
            _ => return usage(),
        }
    }
    let argv = o.rest();
    if argv.len() != 2 || flags.contains(BindFlags::AFTER | BindFlags::BEFORE) {
        return usage();
    }
    let (Ok(new), Ok(old)) = (r::cstr(argv[0]), r::cstr(argv[1])) else {
        return usage();
    };
    if r::bind(&new, &old, flags).is_ok() {
        return Ok(());
    }
    if quiet {
        return Ok(());
    }
    // Try to give a less confusing error than the default.
    if r::access(&new, AEXIST).is_err() {
        r::eprintln!("bind: {}: {Rerr}", Text(argv[0]));
    } else if r::access(&old, AEXIST).is_err() {
        r::eprintln!("bind: {}: {Rerr}", Text(argv[1]));
    } else {
        r::eprintln!("bind {} {}: {Rerr}", Text(argv[0]), Text(argv[1]));
    }
    Err("bind")
}
