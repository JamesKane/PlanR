//! `du [-aefhnqstu] [-b size] [-p si-pfx] [file ...]`: the disk space
//! that each file, or each directory and the directories in it, take,
//! in kbytes, reckoned in blocks of `size` (1024; each `k` after it
//! multiplies by 1024) (Plan 9's `du.c`). `-a` lists files too, `-s`
//! only the top; `-n` gives bytes (and lists files), `-p` scales by an
//! SI prefix (`k`, `M`, …, each 1024), `-e` prints in `%g` style, `-h`
//! picks the prefix itself; `-t` gives the modification time instead,
//! `-u` with it the access time, `-q` the qid path; `-f` keeps quiet
//! about errors; `-r` (undocumented) reads every file and prints
//! nothing. Names are quoted as rc would.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use r::num::{FloatSpec, format_float};
use r::{Access, Bout, DirBuf, File, OpenFlags, OpenMode, RcQuoted, Text};

r::main!(run);

const VKILO: u64 = 1024;

/// The SI prefixes for units above 1.
const PFXES: [&str; 9] = ["", "k", "M", "G", "T", "P", "E", "Z", "Y"];

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // the flags
struct Du {
    aflag: bool,
    autoscale: bool,
    fflag: bool,
    fltflag: bool,
    qflag: bool,
    readflg: bool,
    sflag: bool,
    tflag: bool,
    uflag: bool,
    blocksize: u64,
    /// What the output is counted in.
    unit: u64,
    /// The directories seen, by qid path, type and device.
    seen: Vec<(u64, u16, u32)>,
    out: Option<Bout<File>>,
}

fn usage() -> ! {
    r::eprintln!("usage: du [-aefhnqstu] [-b size] [-p si-pfx] [file ...]");
    r::exits(Some(b"usage"))
}

/// `%.6g`.
fn g6(v: f64) -> alloc::string::String {
    let spec = FloatSpec {
        prec: Some(6),
        verb: b'g',
        ..FloatSpec::default()
    };
    format_float(&spec, v)
}

impl Du {
    #[allow(clippy::cast_precision_loss)] // as C's (double)
    fn printamt(&mut self, amt: u64, name: &[u8]) {
        if self.readflg {
            return;
        }
        let name = RcQuoted(name);
        let line = if self.autoscale {
            let mut val = amt as f64 / self.unit as f64;
            let mut scale = 0;
            while val.abs() >= 1024.0 && scale < PFXES.len() - 1 {
                scale += 1;
                val /= 1024.0;
            }
            alloc::format!("{}{}\t{name}\n", g6(val), PFXES[scale])
        } else if self.fltflag {
            alloc::format!("{}\t{name}\n", g6(amt as f64 / self.unit as f64))
        } else if self.qflag {
            alloc::format!("{:016x}\t{name}\n", amt.div_ceil(self.unit))
        } else {
            alloc::format!("{}\t{name}\n", amt.div_ceil(self.unit))
        };
        if let Some(out) = self.out.as_mut() {
            let _ = out.write(line.as_bytes());
        }
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

    /// `n` rounded up to whole blocks.
    fn blkmultiple(&self, n: u64) -> u64 {
        if self.blocksize == 1 {
            return n;
        }
        n.div_ceil(self.blocksize) * self.blocksize
    }

    /// What is reported for `d`, of `size`.
    fn dirval(&self, d: &DirBuf, size: u64) -> u64 {
        if self.qflag {
            d.qid.path
        } else if self.tflag && self.uflag {
            u64::from(d.atime)
        } else if self.tflag {
            u64::from(d.mtime)
        } else {
            size
        }
    }

    fn readfile(&mut self, name: &[u8]) {
        let mode = OpenMode {
            access: Access::Read,
            flags: OpenFlags::empty(),
        };
        let fd = match r::cstr(name).and_then(|c| r::open(&c, mode)) {
            Ok(fd) => fd,
            Err(e) => {
                self.warn(name, &e);
                return;
            }
        };
        let mut buf = alloc::vec![0u8; usize::try_from(self.blocksize).unwrap_or(8192).min(1 << 20)];
        loop {
            match r::read(fd, &mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    self.warn(name, &e);
                    break;
                }
            }
        }
        let _ = r::close(fd);
    }

    fn dufile(&mut self, name: &[u8], d: &DirBuf) -> u64 {
        let mut t = self.blkmultiple(d.length);
        if self.aflag || self.readflg {
            let file = [name, b"/", d.name.as_bytes()].concat();
            if self.readflg {
                self.readfile(&file);
            }
            t = self.dirval(d, t);
            self.printamt(t, &file);
        }
        t
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
        if !dir.is_dir() {
            return self.dirval(&dir, self.blkmultiple(dir.length));
        }
        let mode = OpenMode {
            access: Access::Read,
            flags: OpenFlags::empty(),
        };
        let fd = match r::cstr(name).and_then(|c| r::open(&c, mode)) {
            Ok(fd) => fd,
            Err(e) => return self.warn(name, &e),
        };
        let mut nk = 0;
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
                if !d.is_dir() {
                    nk += self.dufile(name, &d);
                    continue;
                }
                if d.name == "." || d.name == ".." || self.seen(&d) {
                    continue;
                }
                let file = [name, b"/", d.name.as_bytes()].concat();
                let t = self.du(&file, Ok(d.clone()));
                nk += t;
                let t = self.dirval(&d, t);
                if !self.sflag {
                    self.printamt(t, &file);
                }
            }
        }
        let _ = r::close(fd);
        self.dirval(&dir, nk)
    }
}

fn stat(name: &[u8]) -> Result<DirBuf, r::Error> {
    r::cstr(name).and_then(|c| r::dirstat(&c))
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut du = Du {
        blocksize: VKILO,
        out: Some(Bout::new(r::STDOUT)),
        ..Du::default()
    };
    while let Some(c) = o.next() {
        match c {
            'a' => du.aflag = true,
            'b' => {
                if let Some(s) = o.arg() {
                    let (n, k) = r::num::strtoul(s, 0);
                    du.blocksize = if k == 0 { 1 } else { n };
                    // Each k, and the byte after the last, as Plan 9's
                    // `while(*ss++ == 'k')`.
                    for _ in s[k..].iter().take_while(|&&c| c == b'k') {
                        du.blocksize = du.blocksize.wrapping_mul(1024);
                    }
                }
            }
            'e' => du.fltflag = true,
            'f' => du.fflag = true,
            'h' => du.autoscale = true,
            'n' => {
                du.aflag = true;
                du.blocksize = 1;
                du.unit = 1;
            }
            'p' => {
                if let Some(s) = o.arg() {
                    let Some(scale) = PFXES.iter().position(|p| p.as_bytes().eq_ignore_ascii_case(s)) else {
                        // sysfatal: the message is the status too.
                        let msg = [&b"unknown suffix "[..], s].concat();
                        r::eprintln!("du: {}", Text(&msg));
                        r::exits(Some(&msg));
                    };
                    du.unit = 1;
                    for _ in 0..scale {
                        du.unit = du.unit.wrapping_mul(VKILO);
                    }
                }
            }
            'q' => du.qflag = true,
            'r' => du.readflg = true,
            's' => du.sflag = true,
            't' => du.tflag = true,
            'u' => du.uflag = true,
            _ => usage(),
        }
    }
    if du.unit == 0 {
        du.unit = if du.qflag || du.tflag || du.uflag || du.autoscale {
            1
        } else {
            VKILO
        };
    }
    du.blocksize = du.blocksize.max(1);
    let files = o.rest();
    if files.is_empty() {
        let n = du.du(b".", stat(b"."));
        du.printamt(n, b".");
    }
    for f in files {
        let n = du.du(f, stat(f));
        du.printamt(n, f);
    }
    if let Some(mut out) = du.out.take() {
        out.flush().map_err(|_| "write error")?;
    }
    Ok(())
}
