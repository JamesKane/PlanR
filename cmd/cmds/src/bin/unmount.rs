//! `unmount [mounted] mountpoint`: undoes a `bind` or `mount` on
//! `mountpoint`: of `mounted` only, if it is given, as Plan 9's
//! `unmount`.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use r::{Rerr, Text};

r::main!(run);

fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let argv0 = cmdline.get(0).unwrap_or(b"unmount");
    let argv: Vec<&[u8]> = cmdline.iter().skip(1).collect();
    let (mnted, mtpt) = match argv.as_slice() {
        [mtpt] => (None, *mtpt),
        [mnted, mtpt] => (Some(*mnted), *mtpt),
        _ => {
            r::eprintln!("usage: unmount mountpoint");
            r::eprintln!("       unmount mounted mountpoint");
            return Err("usage");
        }
    };
    // Unmount takes its arguments in the same order as mount.
    let done = r::cstr(mtpt).and_then(|old| {
        let new = mnted.map(r::cstr).transpose()?;
        r::unmount(new.as_deref(), &old)
    });
    if done.is_err() {
        // sysfatal, named as invoked: the message is the status too.
        let msg = format!("{}: {Rerr}", Text(mtpt));
        r::eprintln!("{}: {msg}", Text(argv0));
        r::exits(Some(msg.as_bytes()));
    }
    Ok(())
}
