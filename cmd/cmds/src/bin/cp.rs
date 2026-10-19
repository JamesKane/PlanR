//! `cp [-gux] fromfile tofile`, `cp [-x] fromfile ... todir`: copies
//! files, to a new name or into a directory, keeping their permissions.
//! `-x` keeps the modification time and mode too, `-g` the group, `-u`
//! the owner and group (as `wstat` allows).

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use r::{Access, DirBuf, Error, OpenFlags, OpenMode, Text};

r::main!(run);

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // the flags
struct Cp {
    gflag: bool,
    uflag: bool,
    xflag: bool,
    failed: bool,
}

fn stat(name: &[u8]) -> Result<DirBuf, Error> {
    r::cstr(name).and_then(|c| r::dirstat(&c))
}

fn same(a: &DirBuf, b: &DirBuf) -> bool {
    a.qid == b.qid && a.dev == b.dev && a.ty == b.ty
}

impl Cp {
    fn fail(&mut self) {
        self.failed = true;
    }

    fn copy(&mut self, from: &[u8], to: &[u8], todir: bool) {
        let to: Vec<u8> = if todir {
            let elem = from.iter().rposition(|&b| b == b'/').map_or(from, |i| &from[i + 1..]);
            [to, b"/", elem].concat()
        } else {
            to.to_vec()
        };
        let (ft, tt) = (Text(from), Text(&to));
        let d = match stat(from) {
            Ok(d) => d,
            Err(e) => {
                r::eprintln!("cp: can't stat {ft}: {e}");
                return self.fail();
            }
        };
        if d.is_dir() {
            r::eprintln!("cp: {ft} is a directory");
            return self.fail();
        }
        if stat(&to).is_ok_and(|t| same(&d, &t)) {
            r::eprintln!("cp: {ft} and {tt} are the same file");
            return self.fail();
        }
        let read = OpenMode {
            access: Access::Read,
            flags: OpenFlags::empty(),
        };
        let fdf = match r::cstr(from).and_then(|c| r::open(&c, read)) {
            Ok(fd) => fd,
            Err(e) => {
                r::eprintln!("cp: can't open {ft}: {e}");
                return self.fail();
            }
        };
        let write = OpenMode {
            access: Access::Write,
            flags: OpenFlags::empty(),
        };
        let fdt = match r::cstr(&to).and_then(|c| r::create(&c, write, d.mode & 0o777)) {
            Ok(fd) => fd,
            Err(e) => {
                r::eprintln!("cp: can't create {tt}: {e}");
                let _ = r::close(fdf);
                return self.fail();
            }
        };
        if self.copy1(fdf, fdt, from, &to) && (self.xflag || self.gflag || self.uflag) {
            let mut n = DirBuf::null();
            if self.xflag {
                n.mtime = d.mtime;
                n.mode = d.mode;
            }
            if self.uflag {
                n.uid.clone_from(&d.uid);
            }
            if self.gflag {
                n.gid.clone_from(&d.gid);
            }
            if let Err(e) = r::dirfwstat(fdt, &n) {
                r::eprintln!("cp: warning: can't wstat {tt}: {e}");
            }
        }
        let _ = r::close(fdf);
        let _ = r::close(fdt);
    }

    /// Copies what `fdf` holds to `fdt`; false (having said why) if it
    /// cannot.
    fn copy1(&mut self, fdf: r::Fd, fdt: r::Fd, from: &[u8], to: &[u8]) -> bool {
        let mut buf = alloc::vec![0u8; 8192];
        loop {
            match r::read(fdf, &mut buf) {
                Ok(0) => return true,
                Ok(n) => match r::write(fdt, &buf[..n]) {
                    Ok(m) if m == n => {}
                    Ok(_) => return self.write_error(to, &Error::from_text(b"short write")),
                    Err(e) => return self.write_error(to, &e),
                },
                Err(e) => {
                    r::eprintln!("cp: error reading {}: {e}", Text(from));
                    self.fail();
                    return false;
                }
            }
        }
    }

    fn write_error(&mut self, to: &[u8], e: &Error) -> bool {
        r::eprintln!("cp: error writing {}: {e}", Text(to));
        self.fail();
        false
    }
}

fn usage() -> Result<(), &'static str> {
    r::eprintln!("usage:\tcp [-gux] fromfile tofile");
    r::eprintln!("\tcp [-x] fromfile ... todir");
    Err("usage")
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut cp = Cp::default();
    while let Some(c) = o.next() {
        match c {
            'g' => cp.gflag = true,
            'u' => {
                cp.uflag = true;
                cp.gflag = true;
            }
            'x' => cp.xflag = true,
            _ => return usage(),
        }
    }
    let files = o.rest();
    let Some((&to, from)) = files.split_last().filter(|(_, f)| !f.is_empty()) else {
        return usage();
    };
    let todir = stat(to).is_ok_and(|d| d.is_dir());
    if from.len() > 1 && !todir {
        r::eprintln!("cp: {} not a directory", Text(to));
        return Err("bad usage");
    }
    for f in from {
        cp.copy(f, to, todir);
    }
    if cp.failed { Err("errors") } else { Ok(()) }
}
