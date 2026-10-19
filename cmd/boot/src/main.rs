//! `/boot/boot`, the first user process, as Plan 9's `boot` (`9/boot/
//! boot.c`) for a machine whose root is the boot archive: the kernel
//! starts it in a name space of nothing but `#/`, which holds the
//! archive, with the environment Plan 9's `init0` sets (`cputype`,
//! `service`, `terminal` and the boot configuration's settings). It opens
//! the console as descriptors 0, 1 and 2, binds the environment and
//! `#s`, and runs init: `/$cputype/init -t` (`-tm` with `-m`), or the
//! command in `$init`, split as `tokenize` splits it.
//!
//! What Plan 9's does to find, authenticate to and mount a root file
//! server, set the time from it and start swapping is not needed: the
//! root is already there, the clock is set by the kernel, and there is no
//! swap.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::ffi::CString;
use alloc::format;
use alloc::vec::Vec;
use core::ffi::CStr;

use r::{Access, BindFlags, OpenFlags, OpenMode, Rerr, Text};

r::main!(run);

const fn mode(access: Access) -> OpenMode {
    OpenMode {
        access,
        flags: OpenFlags::empty(),
    }
}

/// We should inherit the standard descriptors all referring to
/// `/dev/cons`, but are paranoid.
fn opencons() {
    for fd in 0..3 {
        let _ = r::close(fd);
    }
    let _ = r::bind(c"#c", c"/dev", BindFlags::BEFORE);
    let _ = r::open(c"/dev/cons", mode(Access::Read));
    let _ = r::open(c"/dev/cons", mode(Access::Write));
    let _ = r::open(c"/dev/cons", mode(Access::Write));
}

/// Init will build its name space anew; `#ec` would give the boot
/// configuration's `*` settings, which there are none of yet.
fn bindenvsrv() {
    let _ = r::bind(c"#ec", c"/env", BindFlags::empty());
    let _ = r::bind(c"#e", c"/env", BindFlags::BEFORE | BindFlags::CREATE);
    let _ = r::bind(c"#s", c"/srv/", BindFlags::CREATE);
}

/// The first line of `name`'s contents, or nothing.
fn readfile(name: &CStr) -> Vec<u8> {
    let Ok(fd) = r::open(name, mode(Access::Read)) else {
        return Vec::new();
    };
    let mut buf = [0u8; 64];
    let n = r::read(fd, &mut buf[..63]).unwrap_or(0);
    let _ = r::close(fd);
    buf[..n].to_vec()
}

fn fatal(s: &[u8]) -> ! {
    let msg = format!("{}: {Rerr}", Text(s));
    r::eprintln!("boot: {msg}");
    r::exits(Some(msg.as_bytes()))
}

fn run(args: r::Args) -> Result<(), &'static str> {
    opencons();
    bindenvsrv();
    let mut o = r::Opts::new(args.iter());
    let mut mflag = false;
    while let Some(c) = o.next() {
        // -k and -f choose how the root server is reached: not needed.
        if c == 'm' {
            mflag = true;
        }
    }
    let cputype = readfile(c"#e/cputype");
    execinit(&cputype, mflag)
}

/// Runs init, or `$init`.
fn execinit(cputype: &[u8], mflag: bool) -> ! {
    let cmd = match r::getenv(b"init") {
        Some(c) => c,
        None => format!("/{}/init -t{}", Text(cputype), if mflag { "m" } else { "" }).into_bytes(),
    };
    let mut iargv = r::tokenize(&cmd);
    iargv.truncate(15);
    let Some(file) = iargv.first().cloned() else {
        fatal(&cmd)
    };
    // Make iargv[0] the last element of the name.
    if let Some(i) = file.iter().rposition(|&c| c == b'/') {
        iargv[0] = file[i + 1..].to_vec();
    }
    let cargs: Vec<CString> = iargv.into_iter().filter_map(|a| CString::new(a).ok()).collect();
    let argv: Vec<&CStr> = cargs.iter().map(CString::as_c_str).collect();
    if let Ok(f) = CString::new(file.as_slice()) {
        let _ = r::exec(&f, &argv);
    }
    fatal(&file)
}
