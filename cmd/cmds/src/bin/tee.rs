//! `tee [-ai] [file ...]`: copies standard input to standard output and
//! to each file, truncated or, with `-a`, appended to. `-i` ignores
//! interrupts.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use r::{Access, NoteHow, OpenFlags, OpenMode, Text, Ureg, Whence};

r::main!(run);

extern "C" fn ignore_interrupt(_: *mut Ureg, msg: *const u8) {
    // SAFETY: the kernel passes the note's NUL-terminated text.
    let msg = unsafe { core::ffi::CStr::from_ptr(msg.cast()) };
    let how = if msg.to_bytes() == b"interrupt" {
        NoteHow::Cont
    } else {
        NoteHow::Dflt
    };
    let _ = r::noted(how);
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut aflag = false;
    while let Some(c) = o.next() {
        match c {
            'a' => aflag = true,
            'i' => {
                let _ = r::notify(Some(ignore_interrupt));
            }
            // A relic from Unix, ignored.
            'u' => {}
            _ => {
                r::eprintln!("usage: tee [-ai] [file ...]");
                return Err("usage");
            }
        }
    }
    let write = OpenMode {
        access: Access::Write,
        flags: OpenFlags::empty(),
    };
    let mut fds: Vec<r::Fd> = Vec::new();
    for f in o.rest() {
        let fd = r::cstr(f).and_then(|c| {
            if aflag {
                let fd = r::open(&c, write).or_else(|_| r::create(&c, write, 0o666))?;
                let _ = r::seek(fd, 0, Whence::End);
                Ok(fd)
            } else {
                r::create(&c, write, 0o666)
            }
        });
        match fd {
            Ok(fd) => fds.push(fd),
            Err(e) => r::eprintln!("tee: cannot open {}: {e}", Text(f)),
        }
    }
    fds.push(1);
    let mut buf = alloc::vec![0u8; 8192];
    loop {
        let n = match r::read(0, &mut buf) {
            Ok(n) if n > 0 => n,
            _ => return Ok(()),
        };
        for &fd in &fds {
            let _ = r::write(fd, &buf[..n]);
        }
    }
}
