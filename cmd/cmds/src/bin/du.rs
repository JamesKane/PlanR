//! `du [-afnqstu] [-b size] [file ...]`: the disk space, in kbytes, that
//! each file, or each directory and the directories in it, take, reckoned
//! in blocks of `size` (1024; `k` after it multiplies by 1024). `-a`
//! lists files too, `-s` only the top; `-n` gives bytes (and lists files),
//! `-t` the modification time instead, `-u` with it the access time, `-q`
//! the qid path; `-f` keeps quiet about errors.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use r::{Access, Bout, DirBuf, File, OpenFlags, OpenMode, Text};

r::main!(run);

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // the flags
struct Du {
    aflag: bool,
    fflag: bool,
    nflag: bool,
    sflag: bool,
    tflag: bool,
    uflag: bool,
    qflag: bool,
    blocksize: u64,
    /// The directories seen, by qid path, type and device.
    seen: Vec<(u64, u16, u32)>,
    out: Option<Bout<File>>,
}

impl Du {
    fn print(&mut self, n: u64, name: &[u8]) {
        let Some(out) = self.out.as_mut() else { return };
        let _ = if self.qflag {
            out.print(format_args!("{n:016x}\t{}\n", Text(name)))
        } else {
            out.print(format_args!("{n}\t{}\n", Text(name)))
        };
    }

    fn warn(&mut self, name: &[u8], e: &r::Error) -> u64 {
        if !self.fflag {
            if let Some(o) = self.out.as_mut() {
                let _ = o.flush();
            }
            r::eprintln!("du: {}: {e}", Text(name));
        }
        0
    }

    fn k(&self, n: u64) -> u64 {
        if self.nflag {
            return n;
        }
        n.div_ceil(self.blocksize) * self.blocksize / 1024
    }

    /// What `-t`, `-u` and `-q` print on the line for `d`, or `n`.
    fn value(&self, d: &DirBuf, n: u64) -> u64 {
        if self.qflag {
            d.qid.path
        } else if self.tflag && self.uflag {
            u64::from(d.atime)
        } else if self.tflag {
            u64::from(d.mtime)
        } else {
            n
        }
    }

    fn seen(&mut self, d: &DirBuf) -> bool {
        let key = (d.qid.path, d.ty, d.dev);
        if self.seen.contains(&key) {
            return true;
        }
        self.seen.push(key);
        false
    }

    fn du(&mut self, name: &[u8], dir: Result<DirBuf, r::Error>) -> u64 {
        let dir = match dir {
            Ok(d) => d,
            Err(e) => return self.warn(name, &e),
        };
        let mode = OpenMode {
            access: Access::Read,
            flags: OpenFlags::empty(),
        };
        let fd = match r::cstr(name).and_then(|c| r::open(&c, mode)) {
            Ok(fd) => fd,
            Err(e) => return self.warn(name, &e),
        };
        let mut nk = 0;
        if dir.is_dir() {
            loop {
                let entries = match r::dirread(fd) {
                    Ok(e) if e.is_empty() => break,
                    Ok(e) => e,
                    Err(e) => {
                        self.warn(name, &e);
                        break;
                    }
                };
                for d in entries {
                    let file = [name, b"/", d.name.as_bytes()].concat();
                    if !d.is_dir() {
                        let t = self.k(d.length);
                        nk += t;
                        if self.aflag {
                            let v = self.value(&d, t);
                            self.print(v, &file);
                        }
                        continue;
                    }
                    if d.name == "." || d.name == ".." || self.seen(&d) {
                        continue;
                    }
                    let t = self.du(&file, Ok(d.clone()));
                    nk += t;
                    if !self.sflag {
                        let v = self.value(&d, t);
                        self.print(v, &file);
                    }
                }
            }
        } else {
            nk = self.k(dir.length);
        }
        let _ = r::close(fd);
        // Here, unlike in the lines above, -t wins over -q.
        if self.tflag {
            return u64::from(if self.uflag { dir.atime } else { dir.mtime });
        }
        self.value(&dir, nk)
    }
}

fn stat(name: &[u8]) -> Result<DirBuf, r::Error> {
    r::cstr(name).and_then(|c| r::dirstat(&c))
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut du = Du {
        blocksize: 1024,
        out: Some(Bout::new(r::STDOUT)),
        ..Du::default()
    };
    while let Some(c) = o.next() {
        match c {
            'a' => du.aflag = true,
            's' => du.sflag = true,
            'f' => du.fflag = true,
            'n' => {
                du.aflag = true;
                du.nflag = true;
            }
            't' => du.tflag = true,
            'u' => du.uflag = true,
            'q' => du.qflag = true,
            'b' => {
                if let Some(s) = o.arg() {
                    let (n, k) = r::num::strtoul(s, 0);
                    du.blocksize = if k == 0 { 1 } else { n };
                    if s.get(k) == Some(&b'k') {
                        du.blocksize = du.blocksize.wrapping_mul(1024);
                    }
                    du.blocksize = du.blocksize.max(1);
                }
            }
            _ => {}
        }
    }
    let files = o.rest();
    if files.is_empty() {
        let n = du.du(b".", stat(b"."));
        du.print(n, b".");
    }
    for f in files {
        let n = du.du(f, stat(f));
        du.print(n, f);
    }
    if let Some(mut out) = du.out.take() {
        out.flush().map_err(|_| "write error")?;
    }
    Ok(())
}
