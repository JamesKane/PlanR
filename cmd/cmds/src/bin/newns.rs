//! `auth/newns [-ad] [-n namespace] [cmd [args ...]]`: runs `cmd` (`rc
//! -i` if none) in a new name space built from `/lib/namespace` (or
//! `namespace`), as Plan 9's `auth/newns`; `-a` adds the file's commands
//! to the name space there is instead, `-d` shows them as they run. A
//! command not found as named is looked for in `/bin`.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::ffi::CString;
use alloc::vec::Vec;
use core::ffi::CStr;

use r::{RforkFlags, Text};

r::main!(run);

fn usage() -> ! {
    r::eprintln!("usage: newns [-ad] [-n namespace] [cmd [args...]]");
    r::exits(Some(b"usage"))
}

/// Whether `s` names a file from the root or the current directory.
fn rooted(s: &[u8]) -> bool {
    s.starts_with(b"/") || s.starts_with(b"./") || s.starts_with(b"../")
}

fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let _ = r::rfork(RforkFlags::NAMEG);
    let mut o = r::Opts::new(cmdline.iter());
    let mut add = false;
    let mut nsfile: &[u8] = b"/lib/namespace";
    while let Some(c) = o.next() {
        match c {
            'a' => add = true,
            'd' => r::ns::set_debug(true),
            'n' => nsfile = o.arg().unwrap_or(b""),
            _ => usage(),
        }
    }
    let argv0 = o.argv0();
    let mut argv: Vec<&[u8]> = o.rest();
    if argv.is_empty() {
        argv = alloc::vec![b"/bin/rc", b"-i"];
    }
    let user = r::read_all(c"/dev/user").unwrap_or_default();
    if let Ok(file) = CString::new(nsfile) {
        let _ = if add {
            r::addns(&user, &file)
        } else {
            r::newns(&user, Some(&file))
        };
    }
    let owned: Vec<CString> = argv.iter().filter_map(|a| CString::new(*a).ok()).collect();
    let cargv: Vec<&CStr> = owned.iter().map(CString::as_c_str).collect();
    // The first error is the one reported.
    let e = r::exec(cargv[0], &cargv);
    if !rooted(argv[0])
        && let Ok(n) = CString::new([&b"/bin/"[..], argv[0]].concat())
    {
        let _ = r::exec(&n, &cargv);
    }
    // sysfatal: the message is the status too.
    let msg = alloc::format!("exec: {}: {e}", Text(argv[0]));
    r::eprintln!("{}: {msg}", Text(argv0));
    r::exits(Some(msg.as_bytes()))
}
