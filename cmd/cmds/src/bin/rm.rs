//! `rm [-fr] file ...`: removes each file; with `-r`, a directory and
//! all in it. `-f` keeps quiet about what cannot be removed. The exit
//! status is the last error.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use r::{Access, Error, OpenFlags, OpenMode, Text};

r::main!(run);

struct Rm {
    quiet: bool,
    /// The last error.
    status: Vec<u8>,
}

impl Rm {
    fn err(&mut self, f: &[u8], e: &Error) {
        if !self.quiet {
            self.status = e.as_bytes().to_vec();
            r::eprintln!("rm: {}: {e}", Text(f));
        }
    }

    /// Removes `f`, a directory that is not empty, and what is in it.
    fn rmdir(&mut self, f: &[u8]) {
        let mode = OpenMode {
            access: Access::Read,
            flags: OpenFlags::empty(),
        };
        let entries = match r::cstr(f).and_then(|c| r::open(&c, mode)) {
            Ok(fd) => {
                let d = r::dirreadall(fd);
                let _ = r::close(fd);
                match d {
                    Ok(d) => d,
                    Err(e) => return self.err(b"dirreadall", &e),
                }
            }
            Err(e) => return self.err(f, &e),
        };
        let mut dirs = Vec::new();
        for d in &entries {
            let name = [f, b"/", d.name.as_bytes()].concat();
            match r::cstr(&name).and_then(|c| r::remove(&c)) {
                Ok(()) => {}
                Err(_) if d.is_dir() => dirs.push(name),
                Err(e) => self.err(&name, &e),
            }
        }
        for d in dirs {
            self.rmdir(&d);
        }
        if let Err(e) = r::cstr(f).and_then(|c| r::remove(&c)) {
            self.err(f, &e);
        }
    }
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut recurse = false;
    let mut rm = Rm {
        quiet: false,
        status: Vec::new(),
    };
    while let Some(c) = o.next() {
        match c {
            'r' => recurse = true,
            'f' => rm.quiet = true,
            _ => {
                r::eprintln!("usage: rm [-fr] file ...");
                return Err("usage");
            }
        }
    }
    for f in o.rest() {
        let c = match r::cstr(f) {
            Ok(c) => c,
            Err(e) => {
                rm.err(f, &e);
                continue;
            }
        };
        let Err(e) = r::remove(&c) else { continue };
        if !recurse {
            rm.err(f, &e);
            continue;
        }
        match r::dirstat(&c) {
            Ok(d) if d.is_dir() => rm.rmdir(f),
            Ok(_) => rm.err(f, &e),
            Err(e) => rm.err(f, &e),
        }
    }
    if rm.status.is_empty() {
        Ok(())
    } else {
        r::exits(Some(&rm.status))
    }
}
