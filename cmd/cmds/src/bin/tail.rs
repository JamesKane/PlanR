//! `tail [-n N] [-c N] [-f] [-r] [+-N[bc][fr]] [file]`: the end of a
//! file, or standard input: its last 10 lines, or the last (`-N`) or all
//! from the start but the first (`+N`) N lines, characters (`c`) or
//! 1024-byte blocks (`b`); `-r` prints lines in reverse order (all, by
//! default), `-f` waits for more to be written.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use r::{Bout, File, Text, Whence};

r::main!(run);

const BSIZE: usize = r::bio::BSIZE;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Units {
    Chars,
    Lines,
}

#[allow(clippy::struct_excessive_bools)] // the flags
struct Tail {
    count: i64,
    anycount: bool,
    follow: bool,
    /// From the start (`+N`) rather than the end.
    beg: bool,
    units: Units,
    rev: bool,
    file: r::Fd,
    out: Bout<File>,
}

/// A length, as a file offset.
fn off(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn usage() -> ! {
    r::eprintln!("usage: tail [-n N] [-c N] [-f] [-r] [+-N[bc][fr]] [file]");
    r::exits(Some(b"usage"))
}

/// Says `s` and the last error, and exits with `s`.
fn fatal(s: &[u8]) -> ! {
    fail(s, &r::Error::last())
}

/// Says `s` and `e`, and exits with `s`.
fn fail(s: &[u8], e: &r::Error) -> ! {
    r::eprintln!("tail: {}: {e}", Text(s));
    r::exits(Some(s))
}

impl Tail {
    /// Takes the count in `s` (after a sign, which if `+` counts from
    /// the start); false if there are no digits.
    fn getnumber(&mut self, s: &[u8], before: u8) -> bool {
        let (prev, s) = match s.first() {
            Some(&c @ (b'-' | b'+')) => (c, &s[1..]),
            _ => (before, s),
        };
        if !s.first().is_some_and(u8::is_ascii_digit) {
            return false;
        }
        if prev == b'+' {
            self.beg = true;
        }
        if self.anycount {
            fatal(b"excess option");
        }
        self.anycount = true;
        self.count = r::num::atol(s);
        if self.count < 0 || i32::try_from(self.count).is_err() {
            fatal(b"too big");
        }
        true
    }

    /// The `b`, `c`, `l`, `r` and `f` after a count.
    fn suffix(&mut self, s: &[u8]) {
        let mut s = s;
        while s.first().is_some_and(|c| b"0123456789+-".contains(c)) {
            s = &s[1..];
        }
        match s.first() {
            Some(b'b') => {
                self.count = self.count.checked_mul(1024).unwrap_or_else(|| fatal(b"too big"));
                self.units = Units::Chars;
                s = &s[1..];
            }
            Some(b'c') => {
                self.units = Units::Chars;
                s = &s[1..];
            }
            Some(b'l') => s = &s[1..],
            _ => {}
        }
        match s.first() {
            Some(b'r') => self.rev = true,
            Some(b'f') => self.follow = true,
            None => {}
            Some(_) => usage(),
        }
    }

    fn tseek(&self, o: i64, whence: Whence) -> i64 {
        match r::seek(self.file, o, whence) {
            #[allow(clippy::cast_possible_wrap)] // a file offset
            Ok(o) => o as i64,
            Err(e) => fail(b"", &e),
        }
    }

    fn tread(&self, buf: &mut [u8]) -> usize {
        r::read(self.file, buf).unwrap_or_else(|e| fail(b"", &e))
    }

    fn twrite(&mut self, s: &[u8]) {
        if let Err(e) = self.out.write(s) {
            fail(b"", &e);
        }
    }

    fn copy(&mut self) {
        let mut buf = alloc::vec![0u8; BSIZE];
        loop {
            let n = self.tread(&mut buf);
            if n == 0 {
                return;
            }
            self.twrite(&buf[..n]);
            let _ = self.out.flush();
        }
    }

    /// Reads past the head of the file to the tail.
    fn skip(&mut self) {
        let mut buf = alloc::vec![0u8; BSIZE];
        if self.units == Units::Chars {
            while self.count > 0 {
                let n = usize::try_from(self.count).unwrap_or(BSIZE).min(BSIZE);
                let n = self.tread(&mut buf[..n]);
                if n == 0 {
                    return;
                }
                self.count -= off(n);
            }
        } else {
            let (mut n, mut i) = (0, 0);
            while self.count > 0 {
                n = self.tread(&mut buf);
                if n == 0 {
                    return;
                }
                i = 0;
                while i < n && self.count > 0 {
                    if buf[i] == b'\n' {
                        self.count -= 1;
                    }
                    i += 1;
                }
            }
            self.twrite(&buf[i..n]);
        }
        self.copy();
    }

    /// Reads the whole file, keeping its tail: for what cannot seek.
    fn keep(&mut self) {
        let mut buf = Vec::new();
        let mut chunk = alloc::vec![0u8; BSIZE];
        loop {
            let n = self.tread(&mut chunk);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let count = usize::try_from(self.count).unwrap_or(usize::MAX);
        if count < buf.len() {
            let j = if self.units == Units::Chars {
                buf.len() - count
            } else {
                let mut j = if buf.last() == Some(&b'\n') {
                    buf.len() - 1
                } else {
                    buf.len()
                };
                let mut k = 0;
                while j > 0 {
                    if buf[j - 1] == b'\n' {
                        k += 1;
                        if k >= count {
                            break;
                        }
                    }
                    j -= 1;
                }
                j
            };
            buf.drain(..j);
        }
        let mut len = buf.len();
        if self.rev {
            if len > 0 && buf[len - 1] != b'\n' {
                buf.push(b'\n');
                len += 1;
            }
            let mut j = len.saturating_sub(1);
            while j > 0 {
                if buf[j - 1] == b'\n' {
                    let line = buf[j..len].to_vec();
                    self.twrite(&line);
                    self.count -= 1;
                    if self.count <= 0 {
                        return;
                    }
                    len = j;
                }
                j -= 1;
            }
        }
        if self.count > 0 {
            self.twrite(&buf[..len]);
        }
    }

    /// Counts lines back from the end and prints the tail, forward or
    /// (`-r`) reversed.
    fn reverse(&mut self) {
        let mut buf: Vec<u8> = Vec::new();
        let mut len = 0;
        let mut n = 0;
        let mut pos = self.tseek(0, Whence::End);
        let mut first = true;
        while pos > 0 && self.count > 0 {
            n = usize::try_from(pos).unwrap_or(BSIZE).min(BSIZE);
            pos -= off(n);
            let mut chunk = alloc::vec![0u8; n];
            self.tseek(pos, Whence::Start);
            if self.tread(&mut chunk) != n {
                fatal(b"length error");
            }
            buf.truncate(len);
            chunk.extend_from_slice(&buf);
            buf = chunk;
            len = buf.len();
            if first && buf[len - 1] != b'\n' {
                buf.push(b'\n');
                len += 1;
            }
            first = false;
            n = len - 1;
            while n > 0 && self.count > 0 {
                if buf[n - 1] == b'\n' {
                    self.count -= 1;
                    if self.rev {
                        let line = buf[n..len].to_vec();
                        self.twrite(&line);
                    }
                    len = n;
                }
                n -= 1;
            }
        }
        if !self.rev {
            self.tseek(if n == 0 { 0 } else { pos + off(n) + 1 }, Whence::Start);
            self.copy();
        } else if self.count > 0 {
            let rest = buf[..len].to_vec();
            self.twrite(&rest);
        }
    }

    /// If the file has got shorter than `old`, starts from its start
    /// again; returns how it is now.
    fn trunc(&self, old: Option<&r::DirBuf>) -> Option<r::DirBuf> {
        let mut d = r::dirfstat(self.file).ok()?;
        if d.length < old.map_or(0, |o| o.length) {
            #[allow(clippy::cast_sign_loss)] // 0
            {
                d.length = self.tseek(0, Whence::Start) as u64;
            }
        }
        Some(d)
    }
}

impl Tail {
    /// Reads the options in `argv`; returns where the file name is.
    fn options(&mut self, argv: &[&[u8]]) -> usize {
        let mut i = 1;
        while let Some(&a) = argv.get(i) {
            let Some(&c @ (b'-' | b'+')) = a.first() else {
                break;
            };
            if self.getnumber(a, 0) {
                self.suffix(a);
                i += 1;
                continue;
            }
            if c != b'-' {
                break;
            }
            match a.get(1) {
                Some(&o @ (b'c' | b'n')) => {
                    if o == b'c' {
                        self.units = Units::Chars;
                    }
                    if self.getnumber(&a[2..], o) {
                        i += 1;
                    } else if argv.len() > i + 1 && self.getnumber(argv[i + 1], 0) {
                        i += 2;
                    } else {
                        usage();
                    }
                }
                Some(b'r') => {
                    self.rev = true;
                    i += 1;
                }
                Some(b'f') => {
                    self.follow = true;
                    i += 1;
                }
                Some(b'-') => return i + 1,
                _ => break,
            }
        }
        i
    }
}

fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let argv: Vec<&[u8]> = cmdline.iter().collect();
    let mut t = Tail {
        count: 0,
        anycount: false,
        follow: false,
        beg: false,
        units: Units::Lines,
        rev: false,
        file: 0,
        out: Bout::new(r::STDOUT),
    };
    let i = t.options(&argv);
    if t.rev && (t.units == Units::Chars || t.follow || t.beg) {
        fatal(b"incompatible options");
    }
    if !t.anycount {
        t.count = if t.rev { i64::MAX } else { 10 };
    }
    if t.beg && t.units == Units::Lines && t.count > 0 {
        t.count -= 1;
    }
    let files = &argv[i..];
    if files.len() > 1 {
        usage();
    }
    if let Some(&f) = files.first() {
        let mode = r::OpenMode {
            access: r::Access::Read,
            flags: r::OpenFlags::empty(),
        };
        t.file = r::cstr(f)
            .and_then(|c| r::open(&c, mode))
            .unwrap_or_else(|e| fail(f, &e));
    }
    let seekable = r::seek(t.file, 0, Whence::Start) == Ok(0);
    match (seekable, t.units, t.beg) {
        (false, _, false) => t.keep(),
        (false, _, true) | (true, Units::Lines, true) => t.skip(),
        (true, Units::Chars, false) => {
            t.tseek(-t.count, Whence::End);
            t.copy();
        }
        (true, Units::Chars, true) => {
            t.tseek(t.count, Whence::Start);
            t.copy();
        }
        (true, Units::Lines, false) => t.reverse(),
    }
    if t.follow && seekable {
        let mut sb0 = None;
        let mut sb1: Option<r::DirBuf> = None;
        loop {
            sb0 = t.trunc(sb1.as_ref()).or(sb0);
            t.copy();
            sb1 = t.trunc(sb0.as_ref()).or(sb1);
            let _ = r::sleep(5000);
        }
    }
    t.out.flush().map_err(|_| "write error")
}
