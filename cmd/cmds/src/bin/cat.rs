//! `cat [file ...]`: the files, or standard input, one after another on
//! standard output. A file that cannot be opened, read or copied ends it,
//! as Plan 9's `sysfatal`.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::format;

use r::{Access, OpenFlags, OpenMode};

r::main!(run);

fn fatal(msg: &str) -> ! {
    r::eprintln!("cat: {msg}");
    r::exits(Some(msg.as_bytes()))
}

fn cat(fd: r::Fd, name: &str) {
    let mut buf = alloc::vec![0u8; 8192];
    loop {
        match r::read(fd, &mut buf) {
            Ok(0) => return,
            Ok(n) => match r::write(1, &buf[..n]) {
                Ok(m) if m == n => {}
                Ok(_) => fatal(&format!("write error copying {name}: {}", r::Rerr)),
                Err(e) => fatal(&format!("write error copying {name}: {e}")),
            },
            Err(e) => fatal(&format!("error reading {name}: {e}")),
        }
    }
}

#[allow(clippy::unnecessary_wraps)] // the signature `main!` wants
fn run(args: r::Args) -> Result<(), &'static str> {
    if args.len() == 1 {
        cat(0, "<stdin>");
    }
    for name in args.iter().skip(1) {
        let text = r::print::Text(name);
        let mode = OpenMode {
            access: Access::Read,
            flags: OpenFlags::empty(),
        };
        match r::cstr(name).and_then(|c| r::open(&c, mode)) {
            Ok(fd) => {
                cat(fd, &format!("{text}"));
                let _ = r::close(fd);
            }
            Err(e) => fatal(&format!("can't open {text}: {e}")),
        }
    }
    Ok(())
}
