//! `test expression`: exits normally if the expression is true, else
//! with status `false`. The primaries are POSIX's (`-f -d -r -w -x -e
//! -s -t -n -z`, `= !=`, `-eq -ne -gt -lt -ge -le`, `-nt -ot`) and Plan
//! 9's (`-A -L -T` for append-only, exclusive-use and temporary files,
//! and `file -older time`), joined by `!`, `-a`, `-o` and parentheses.
//! Called `[`, it wants a `]` at the end.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use abi::{DMAPPEND, DMDIR, DMEXCL, DMTMP};
use r::DirBuf;

r::main!(run);

fn synbad(s1: &str, s2: &[u8]) -> ! {
    let mut line = Vec::from(&b"test: "[..]);
    line.extend_from_slice(s1.as_bytes());
    line.extend_from_slice(s2);
    line.push(b'\n');
    let _ = r::write(2, &line);
    r::exits(Some(b"bad syntax"))
}

fn stat(f: &[u8]) -> Option<DirBuf> {
    r::cstr(f).ok().and_then(|c| r::dirstat(&c).ok())
}

fn has_mode(f: &[u8], m: u32) -> bool {
    stat(f).is_some_and(|d| d.mode & m != 0)
}

/// `s` as a whole integer, C's way (an empty `s` is 0).
fn int(s: &[u8]) -> Option<i64> {
    let (v, n) = r::num::strtol(s, 0);
    (n == s.len()).then_some(v)
}

fn isatty(fd: r::Fd) -> bool {
    match (r::dirfstat(fd), r::dirstat(c"/dev/cons")) {
        (Ok(a), Ok(b)) => a.ty == b.ty && a.dev == b.dev && a.qid.path == b.qid.path,
        _ => false,
    }
}

/// Whether `f` was last changed more than `spec` ago: seconds, or a
/// sum of numbers each followed by `y M d h m s`.
fn older(spec: &[u8], file: &[u8]) -> bool {
    let Some(d) = stat(file) else { return false };
    let mut ago: u64 = 0;
    let mut p = spec;
    while !p.is_empty() {
        let (mut m, k) = r::num::strtoul(p, 0);
        p = &p[k..];
        let Some(&unit) = p.first() else {
            ago = m;
            break;
        };
        let scale: &[u64] = match unit {
            b'y' => &[12, 30, 24, 60, 60],
            b'M' => &[30, 24, 60, 60],
            b'd' => &[24, 60, 60],
            b'h' => &[60, 60],
            b'm' => &[60],
            b's' => &[],
            _ => synbad("bad time syntax, ", spec),
        };
        for s in scale {
            m = m.wrapping_mul(*s);
        }
        ago = ago.wrapping_add(m);
        p = &p[1..];
    }
    #[allow(clippy::cast_sign_loss)] // after 1970
    let now = r::time::time() as u64;
    u64::from(d.mtime).wrapping_add(ago) < now
}

/// The arguments, and the next to read.
struct Test<'a> {
    av: Vec<&'a [u8]>,
    ap: usize,
}

impl<'a> Test<'a> {
    /// The next argument; at the end, `None` if `may_end`, else an error.
    fn next(&mut self, may_end: bool) -> Option<&'a [u8]> {
        let a = self.av.get(self.ap).copied();
        if a.is_none() && !may_end {
            synbad("argument expected", b"");
        }
        self.ap += 1;
        a
    }

    fn arg(&mut self) -> &'a [u8] {
        self.next(false).unwrap_or_default()
    }

    fn e(&mut self) -> bool {
        let p1 = self.e1();
        if self.next(true) == Some(b"-o") {
            return p1 || self.e();
        }
        self.ap -= 1;
        p1
    }

    fn e1(&mut self) -> bool {
        let p1 = self.e2();
        if self.next(true) == Some(b"-a") {
            return p1 && self.e1();
        }
        self.ap -= 1;
        p1
    }

    fn e2(&mut self) -> bool {
        if self.arg() == b"!" {
            return !self.e2();
        }
        self.ap -= 1;
        self.e3()
    }

    fn e3(&mut self) -> bool {
        let a = self.arg();
        let tio = |f: &[u8], m| r::cstr(f).is_ok_and(|c| r::access(&c, m).is_ok());
        match a {
            b"(" => {
                let p1 = self.e();
                if self.arg() != b")" {
                    synbad(") expected", b"");
                }
                return p1;
            }
            b"-A" => return has_mode(self.arg(), DMAPPEND),
            b"-L" => return has_mode(self.arg(), DMEXCL),
            b"-T" => return has_mode(self.arg(), DMTMP),
            b"-f" => return stat(self.arg()).is_some_and(|d| d.mode & DMDIR == 0),
            b"-d" => return has_mode(self.arg(), DMDIR),
            b"-r" => return tio(self.arg(), r::AREAD),
            b"-w" => return tio(self.arg(), r::AWRITE),
            b"-x" => return tio(self.arg(), r::AEXEC),
            b"-e" => return tio(self.arg(), r::AEXIST),
            b"-c" | b"-b" | b"-u" | b"-g" => return false,
            b"-s" => return stat(self.arg()).is_some_and(|d| d.length > 0),
            b"-t" => {
                if self.ap >= self.av.len() {
                    return isatty(1);
                }
                match int(self.av[self.ap]).and_then(|n| usize::try_from(n).ok()) {
                    Some(fd) => {
                        self.ap += 1;
                        return isatty(fd);
                    }
                    None => synbad("not a valid file descriptor number ", b""),
                }
            }
            b"-n" => return !self.arg().is_empty(),
            b"-z" => return self.arg().is_empty(),
            _ => {}
        }
        let Some(p2) = self.next(true) else {
            return !a.is_empty();
        };
        match p2 {
            b"=" => return self.arg() == a,
            b"!=" => return self.arg() != a,
            b"-older" => return older(self.arg(), a),
            b"-ot" | b"-nt" => {
                let b = self.arg();
                return match (stat(b), stat(a)) {
                    (Some(bd), Some(ad)) if p2 == b"-ot" => bd.mtime > ad.mtime,
                    (Some(bd), Some(ad)) => bd.mtime < ad.mtime,
                    _ => false,
                };
            }
            _ => {}
        }
        let Some(int1) = int(a) else {
            synbad("unexpected operator/operand: ", p2)
        };
        if let Some(int2) = self.av.get(self.ap).and_then(|s| int(s)) {
            self.ap += 1;
            match p2 {
                b"-eq" => return int1 == int2,
                b"-ne" => return int1 != int2,
                b"-gt" => return int1 > int2,
                b"-lt" => return int1 < int2,
                b"-ge" => return int1 >= int2,
                b"-le" => return int1 <= int2,
                _ => {}
            }
        }
        synbad("unknown operator ", p2)
    }
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut av: Vec<&[u8]> = args.iter().collect();
    if av.first() == Some(&&b"["[..]) {
        if av.last() != Some(&&b"]"[..]) || av.len() < 2 {
            synbad("] missing", b"");
        }
        av.pop();
    }
    if av.len() <= 1 {
        return Err("usage");
    }
    let mut t = Test { av, ap: 1 };
    if t.e() { Ok(()) } else { Err("false") }
}
