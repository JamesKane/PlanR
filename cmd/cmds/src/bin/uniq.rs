//! `uniq [-udc [+-num]] [file]`: copies the file, or standard input,
//! with each run of the same line once, as Plan 9's `uniq.c`. `-u`
//! prints only the lines not repeated, `-d` one copy of those that are,
//! `-c` each with how many times it came; `-s` takes a line that starts
//! with the one before as the same. `-n` skips `n` fields of each line
//! when comparing them, `+n` then `n` bytes. A last line without a
//! newline is not read, as C's `Brdline` does not.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use r::{Access, Bin, Bout, File, OpenFlags, OpenMode, Text};

r::main!(run);

/// The longest line, with its newline.
const SIZE: usize = 8000;

// C's globals.
struct State {
    fields: usize,
    letters: usize,
    linec: i64,
    mode: u8,
    /// The last comparison found the same line.
    uniq: bool,
    fin: Bin<File>,
    fout: Bout<File>,
    argv0: &'static [u8],
}

fn sysfatal(argv0: &[u8], msg: &[u8]) -> ! {
    r::eprintln!("{}: {}", Text(argv0), Text(msg));
    r::exits(Some(msg))
}

impl State {
    /// The next line, without its newline and from any NUL; `None` at
    /// the end, which a line `Brdline` cannot take (with no newline, or
    /// longer than its buffer) is.
    fn gline(&mut self) -> Option<Vec<u8>> {
        let p = self.fin.line(b'\n').ok().flatten()?;
        if p.last() != Some(&b'\n') || p.len() > r::bio::BSIZE {
            return None;
        }
        if p.len() >= SIZE - 1 {
            let argv0 = self.argv0;
            let _ = self.fout.flush();
            sysfatal(argv0, b"line too long");
        }
        let line = &p[..p.len() - 1];
        let end = line.iter().position(|&b| b == 0).unwrap_or(line.len());
        Some(line[..end].to_vec())
    }

    fn pline(&mut self, buf: &[u8]) {
        match self.mode {
            b'u' if self.uniq => {
                self.uniq = false;
                return;
            }
            b'd' if !self.uniq => return,
            b'c' => {
                let _ = self.fout.print(format_args!("{:4} ", self.linec));
            }
            _ => {}
        }
        self.uniq = false;
        let _ = self.fout.write(buf);
        let _ = self.fout.putc(b'\n');
    }

    fn equal(&mut self, b1: &[u8], b2: &[u8]) -> bool {
        let (b1, b2) = if self.fields > 0 || self.letters > 0 {
            (self.skip(b1), self.skip(b2))
        } else {
            (b1, b2)
        };
        let mut i = 0;
        loop {
            let c = b1.get(i).copied().unwrap_or(0);
            if c != b2.get(i).copied().unwrap_or(0) {
                return c == 0 && self.mode == b's';
            }
            if c == 0 {
                self.uniq = true;
                return true;
            }
            i += 1;
        }
    }

    fn skip<'a>(&self, mut s: &'a [u8]) -> &'a [u8] {
        let blank = |s: &[u8]| matches!(s.first(), Some(b' ' | b'\t'));
        for _ in 0..self.fields {
            while blank(s) {
                s = &s[1..];
            }
            while !s.is_empty() && !blank(s) {
                s = &s[1..];
            }
        }
        &s[self.letters.min(s.len())..]
    }

    fn exit(&mut self) -> ! {
        let _ = self.fout.flush();
        r::exits(None)
    }
}

/// C's `atoi`.
fn atoi(s: &[u8]) -> usize {
    usize::try_from(r::num::atol(s)).unwrap_or(0)
}

#[allow(clippy::unnecessary_wraps)] // the signature `main!` wants
fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let argv: Vec<&'static [u8]> = cmdline.iter().collect();
    let argv0 = argv.first().copied().unwrap_or(b"uniq");
    let mut fields = 0;
    let mut letters = 0;
    let mut mode = 0;
    let mut f = 0;
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i];
        match a.first() {
            Some(b'-') => {
                match a.get(1) {
                    Some(d) if d.is_ascii_digit() => fields = atoi(&a[1..]),
                    m => mode = m.copied().unwrap_or(0),
                }
                i += 1;
            }
            Some(b'+') => {
                letters = atoi(&a[1..]);
                i += 1;
            }
            _ => {
                let mode = OpenMode {
                    access: Access::Read,
                    flags: OpenFlags::empty(),
                };
                let Ok(fd) = r::cstr(a).and_then(|c| r::open(&c, mode)) else {
                    sysfatal(argv0, &[&b"cannot open "[..], a].concat())
                };
                f = fd;
                break;
            }
        }
    }
    if argv.len() - i > 1 {
        sysfatal(argv0, &[&b"unexpected argument "[..], argv[i + 1]].concat());
    }
    let mut u = State {
        fields,
        letters,
        linec: 0,
        mode,
        uniq: false,
        fin: Bin::new(File(f)),
        fout: Bout::new(r::STDOUT),
        argv0,
    };

    let Some(mut b1) = u.gline() else { u.exit() };
    loop {
        u.linec += 1;
        let Some(b2) = u.gline() else {
            u.pline(&b1);
            u.exit()
        };
        if !u.equal(&b1, &b2) {
            u.pline(&b1);
            u.linec = 0;
            loop {
                u.linec += 1;
                let Some(next) = u.gline() else {
                    u.pline(&b2);
                    u.exit()
                };
                b1 = next;
                if !u.equal(&b2, &b1) {
                    break;
                }
            }
            u.pline(&b2);
            u.linec = 0;
        }
    }
}
