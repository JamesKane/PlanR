//! `ls [-dlmnpqrstuFQ] [file ...]`: lists each file, or what is in each
//! directory, sorted by name. `-l` gives the long form (mode, device
//! type and number, owner, group, length, time), `-s` sizes in kbytes,
//! `-m` who last changed it, `-q` the qid; `-t` sorts by time, newest
//! first, `-u` takes the access time instead, `-r` reverses, `-n` does
//! not sort; `-d` lists a directory itself, `-p` names without their
//! directories, `-F` marks directories with `/` and executables with
//! `*`, and `-Q` does not quote names as rc would.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt::Write as _;

use r::{Access, Bout, DirBuf, File, Mode, OpenFlags, OpenMode, RcQuoted, Text};

r::main!(run);

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // the flags
struct Flags {
    d: bool,
    l: bool,
    m: bool,
    n: bool,
    p: bool,
    q: bool,
    big_q: bool,
    r: bool,
    s: bool,
    t: bool,
    u: bool,
    big_f: bool,
}

/// An entry to print, and the directory it was found in, if it is to be
/// named so.
struct Entry {
    d: DirBuf,
    prefix: Option<Vec<u8>>,
}

struct Ls {
    f: Flags,
    entries: Vec<Entry>,
    /// The widest of each column so far.
    swidth: usize,
    qwidth: usize,
    vwidth: usize,
    uwidth: usize,
    mwidth: usize,
    glwidth: usize,
    /// The time now, for `-l`.
    clk: i64,
    out: Bout<File>,
}

impl Ls {
    /// Lists `s`; false if it cannot.
    fn ls(&mut self, s: &[u8], multi: bool) -> bool {
        let mut s = s;
        while s.len() > 1 && s.ends_with(b"/") {
            s = &s[..s.len() - 1];
        }
        let fail = |e: r::Error, out: &mut Bout<File>| {
            let _ = out.flush();
            r::eprintln!("ls: {}: {e}", Text(s));
            false
        };
        let c = match r::cstr(s) {
            Ok(c) => c,
            Err(e) => return fail(e, &mut self.out),
        };
        let d = match r::dirstat(&c) {
            Ok(d) => d,
            Err(e) => return fail(e, &mut self.out),
        };
        if d.is_dir() && !self.f.d {
            self.output();
            let mode = OpenMode {
                access: Access::Read,
                flags: OpenFlags::empty(),
            };
            let all = match r::open(&c, mode) {
                Ok(fd) => {
                    let all = r::dirreadall(fd);
                    let _ = r::close(fd);
                    all
                }
                Err(e) => Err(e),
            };
            let all = match all {
                Ok(a) => a,
                Err(e) => return fail(e, &mut self.out),
            };
            for d in all {
                self.entries.push(Entry {
                    d,
                    prefix: multi.then(|| s.to_vec()),
                });
            }
            self.output();
        } else {
            let mut e = Entry { d, prefix: None };
            if let Some(i) = s.iter().rposition(|&b| b == b'/') {
                e.prefix = Some(s[..i].to_vec());
                // The name as given, not as stat gives it.
                e.d.name = String::from_utf8_lossy(&s[i + 1..]).into_owned();
            }
            self.entries.push(e);
        }
        true
    }

    fn output(&mut self) {
        let mut entries = core::mem::take(&mut self.entries);
        if !self.f.n {
            // Ties go in the order the entries came in, and `-r`
            // reverses that too.
            let mut order: Vec<(usize, Entry)> = entries.into_iter().enumerate().collect();
            order.sort_by(|(i, a), (j, b)| {
                let o = self.compare(a, b).then(i.cmp(j));
                if self.f.r { o.reverse() } else { o }
            });
            entries = order.into_iter().map(|(_, e)| e).collect();
        }
        for e in &entries {
            self.widths(&e.d);
        }
        for e in &entries {
            let name = match &e.prefix {
                Some(p) if !self.f.p => {
                    let p: &[u8] = if p == b"/" { b"" } else { p };
                    [p, b"/", e.d.name.as_bytes()].concat()
                }
                _ => e.d.name.as_bytes().to_vec(),
            };
            self.format(&e.d, &name);
        }
        let _ = self.out.flush();
    }

    fn compare(&self, a: &Entry, b: &Entry) -> Ordering {
        if self.f.t {
            let (at, bt) = if self.f.u {
                (a.d.atime, b.d.atime)
            } else {
                (a.d.mtime, b.d.mtime)
            };
            return bt.cmp(&at);
        }
        let (an, bn) = (a.d.name.as_bytes(), b.d.name.as_bytes());
        match (&a.prefix, &b.prefix) {
            (Some(ap), Some(bp)) => ap.cmp(bp).then(an.cmp(bn)),
            (Some(ap), None) => ap[..].cmp(bn).then(Ordering::Greater),
            (None, Some(bp)) => an.cmp(&bp[..]).then(Ordering::Less),
            (None, None) => an.cmp(bn),
        }
    }

    fn widths(&mut self, d: &DirBuf) {
        let width = |n: u64| alloc::format!("{n}").len();
        if self.f.s {
            self.swidth = self.swidth.max(width(d.length.div_ceil(1024)));
        }
        if self.f.q {
            self.qwidth = self.qwidth.max(width(u64::from(d.qid.vers)));
        }
        if self.f.m {
            self.mwidth = self.mwidth.max(d.muid.len() + 2);
        }
        if self.f.l {
            self.vwidth = self.vwidth.max(width(u64::from(d.dev)));
            self.uwidth = self.uwidth.max(d.uid.len());
            self.glwidth = self.glwidth.max(width(d.length) + d.gid.len());
        }
    }

    fn format(&mut self, d: &DirBuf, name: &[u8]) {
        let mut line = String::new();
        if self.f.s {
            let _ = write!(line, "{:>w$} ", d.length.div_ceil(1024), w = self.swidth);
        }
        if self.f.m {
            let _ = write!(line, "{:<w$} ", alloc::format!("[{}]", d.muid), w = self.mwidth);
        }
        if self.f.q {
            let _ = write!(
                line,
                "({:016x} {:>w$} {:02x}) ",
                d.qid.path,
                d.qid.vers,
                d.qid.ty,
                w = self.qwidth
            );
        }
        if self.f.l {
            let t = if self.f.u { d.atime } else { d.mtime };
            let _ = write!(
                line,
                "{} {} {:>vw$} {:<uw$} {} {:>lw$} {} ",
                Mode(d.mode),
                char::from_u32(u32::from(d.ty)).unwrap_or('?'),
                d.dev,
                d.uid,
                d.gid,
                d.length,
                asciitime(i64::from(t), self.clk),
                vw = self.vwidth,
                uw = self.uwidth,
                lw = self.glwidth.saturating_sub(d.gid.len()),
            );
        }
        if self.f.big_q {
            let _ = write!(line, "{}", Text(name));
        } else {
            let _ = write!(line, "{}", RcQuoted(name));
        }
        if self.f.big_f {
            if d.is_dir() {
                line.push('/');
            } else if d.mode & 0o111 != 0 {
                line.push('*');
            }
        }
        line.push('\n');
        let _ = self.out.write(line.as_bytes());
    }
}

/// `t` as `-l` shows it: month, day and time, or month, day and year if
/// it is more than six months ago or a day ahead.
fn asciitime(t: i64, clk: i64) -> String {
    let c = r::time::ctime(t);
    let c = c.as_bytes();
    let mut s = Vec::with_capacity(12);
    if t < clk - 180 * 24 * 60 * 60 || clk + 24 * 60 * 60 < t {
        s.extend_from_slice(c.get(4..11).unwrap_or_default());
        s.extend_from_slice(c.get(23..28).unwrap_or_default());
    } else {
        s.extend_from_slice(c.get(4..16).unwrap_or_default());
    }
    String::from_utf8_lossy(&s).into_owned()
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut f = Flags::default();
    while let Some(c) = o.next() {
        match c {
            'F' => f.big_f = true,
            'd' => f.d = true,
            'l' => f.l = true,
            'm' => f.m = true,
            'n' => f.n = true,
            'p' => f.p = true,
            'q' => f.q = true,
            'Q' => f.big_q = true,
            'r' => f.r = true,
            's' => f.s = true,
            't' => f.t = true,
            'u' => f.u = true,
            _ => {
                r::eprintln!("usage: ls [-dlmnpqrstuFQ] [file ...]");
                return Err("usage");
            }
        }
    }
    let files = o.rest();
    let mut ls = Ls {
        clk: if f.l { r::time::time() } else { 0 },
        f,
        entries: Vec::new(),
        swidth: 0,
        qwidth: 0,
        vwidth: 0,
        uwidth: 0,
        mwidth: 0,
        glwidth: 0,
        out: Bout::new(r::STDOUT),
    };
    let mut ok = true;
    if files.is_empty() {
        ok = ls.ls(b".", false);
    }
    for file in files {
        ok &= ls.ls(file, true);
    }
    ls.output();
    if ok { Ok(()) } else { Err("errors") }
}
