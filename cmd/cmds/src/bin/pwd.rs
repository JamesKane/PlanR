//! `pwd`: the name of the current directory, as the kernel knows it
//! (Plan 9's `getwd`).

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use r::{Access, OpenFlags, OpenMode};

r::main!(run);

fn run(_: r::Args) -> Result<(), &'static str> {
    let mode = OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    };
    let mut buf = [0u8; 512];
    let name = r::open(c".", mode).and_then(|fd| {
        let n = r::fd2path(fd, &mut buf);
        let _ = r::close(fd);
        n
    });
    match name {
        Ok(n) => {
            let mut line = alloc::vec::Vec::from(&buf[..n]);
            line.push(b'\n');
            r::write(1, &line).map_err(|_| "write error")?;
            Ok(())
        }
        Err(e) => {
            r::eprintln!("pwd: {e}");
            Err("getwd")
        }
    }
}
