//! `tr [-cds] [string1 [string2]]`: copies standard input to standard
//! output, translating the runes of `string1` to those of `string2` (the
//! last again if it is shorter), as Plan 9's `tr.c`. `-c` takes the
//! complement of `string1`, `-d` deletes its runes, `-s` squeezes runs of
//! a rune of `string2` (with `-d`, the second string) to one. `a-z` is a
//! range; `\x` and four hexadecimal digits, or `\` and three octal ones,
//! a rune; `\` and any other rune that rune.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use r::{Rerr, Text, utf};

r::main!(run);

const RUNEMAX: u32 = 0x10_ffff;
/// Beyond any rune, as `last` starts.
const NONE: u32 = 0x1_0000;

/// A set of runes.
struct Bits(Vec<u8>);

impl Bits {
    fn new(fill: u8) -> Bits {
        Bits(vec![fill; (RUNEMAX as usize + 1) / 8])
    }

    fn set(&mut self, c: u32) {
        self.0[(c / 8) as usize] |= 1 << (c & 7);
    }

    fn clear(&mut self, c: u32) {
        self.0[(c / 8) as usize] &= !(1 << (c & 7));
    }

    fn get(&self, c: u32) -> bool {
        self.0.get((c / 8) as usize).is_some_and(|b| b & (1 << (c & 7)) != 0)
    }
}

/// A string's parse: where it is, the last rune it gave, and the end of
/// the range it is in.
struct Pcb {
    base: &'static [u8],
    current: usize,
    last: i64,
    fin: i64,
}

impl Pcb {
    fn new(s: &'static [u8]) -> Pcb {
        Pcb {
            base: s,
            current: 0,
            last: -1,
            fin: -1,
        }
    }

    fn rewind(&mut self) {
        self.current = 0;
        self.last = -1;
        self.fin = -1;
    }

    fn at(&self, i: usize) -> u8 {
        self.base.get(i).copied().unwrap_or(0)
    }

    /// The next rune of the string, ranges spelled out; -1 at its end.
    fn canon(&mut self, argv0: &[u8]) -> i64 {
        if self.fin >= 0 {
            if self.last < self.fin {
                self.last += 1;
                return self.last;
            }
            self.fin = -1;
        }
        if self.at(self.current) == 0 {
            return -1;
        }
        if self.at(self.current) == b'-' && self.last >= 0 && self.at(self.current + 1) != 0 {
            let (r, at) = getrune(self.base, self.current + 1, argv0);
            self.current = at;
            if r < self.last {
                sysfatal(argv0, "invalid range specification");
            }
            if r > self.last {
                self.fin = r;
                self.last += 1;
                return self.last;
            }
        }
        let (r, at) = getrune(self.base, self.current, argv0);
        self.current = at;
        self.last = r;
        self.last
    }
}

fn sysfatal(argv0: &[u8], msg: &str) -> ! {
    r::eprintln!("{}: {msg}", Text(argv0));
    r::exits(Some(msg.as_bytes()))
}

/// The rune at `s[i]`: NUL at the end, as C's string has.
fn chartorune(s: &[u8], i: usize) -> (u32, usize) {
    match s.get(i..) {
        Some(rest) if !rest.is_empty() && rest[0] != 0 => {
            let (r, n) = utf::decode(rest);
            (u32::from(r), n)
        }
        _ => (0, 1),
    }
}

/// The rune at `s[i]`, `\` escapes taken; and where the next starts.
fn getrune(s: &[u8], from: usize, argv0: &[u8]) -> (i64, usize) {
    let (r0, len) = chartorune(s, from);
    let mut at = from + len;
    if r0 != u32::from(b'\\') || s.get(at).is_none_or(|&b| b == 0) {
        return (i64::from(r0), at);
    }
    let mut val: i64 = 0;
    if s[at] == b'x' {
        at += 1;
        for i in 0..4 {
            let save = at;
            let (c, len) = chartorune(s, at);
            at += len;
            let d = char::from_u32(c).and_then(|c| c.to_digit(16));
            match d {
                Some(d) => val = 16 * val + i64::from(d),
                None if i == 0 => return (i64::from(b'x'), save),
                None => return (val, save),
            }
        }
    } else {
        for i in 0..3 {
            let save = at;
            let (c, len) = chartorune(s, at);
            at += len;
            match char::from_u32(c).and_then(|c| c.to_digit(8)) {
                Some(d) => val = 8 * val + i64::from(d),
                None if i == 0 => return (i64::from(c), at),
                None => return (val, save),
            }
        }
        if val > 0o377 {
            sysfatal(argv0, "character > 0377");
        }
    }
    (val, at)
}

/// Standard input a rune at a time, and standard output, as `readrune`
/// and `writerune`.
struct Io<'a> {
    argv0: &'a [u8],
    buf: Vec<u8>,
    i: usize,
    n: usize,
    wbuf: Vec<u8>,
}

impl Io<'_> {
    fn readrune(&mut self) -> Option<u32> {
        let mut j = self.i;
        loop {
            if self.i >= self.n {
                self.wflush();
                if j != self.i {
                    self.buf.copy_within(j..self.n, 0);
                }
                self.i = self.n - j;
                let Ok(n) = r::read(0, &mut self.buf[self.i..]) else {
                    let m = alloc::format!("read error: {Rerr}");
                    sysfatal(self.argv0, &m)
                };
                if n == 0 {
                    return None;
                }
                j = 0;
                self.n = n + self.i;
            }
            self.i += 1;
            if utf::full_rune(&self.buf[j..self.i]) {
                break;
            }
        }
        // As C's: the rune is decoded from its first byte, past all the
        // bytes its first said it has.
        Some(u32::from(utf::decode(&self.buf[j..self.i]).0))
    }

    fn writerune(&mut self, r: u32) {
        let c = char::from_u32(r).unwrap_or(utf::RUNE_ERROR);
        let mut b = [0; utf::UTF_MAX];
        let b = c.encode_utf8(&mut b).as_bytes();
        if self.wbuf.len() + b.len() >= 4096 {
            self.wflush();
        }
        self.wbuf.extend_from_slice(b);
    }

    fn wflush(&mut self) {
        if !self.wbuf.is_empty() && r::write(1, &self.wbuf) != Ok(self.wbuf.len()) {
            let m = alloc::format!("write error: {Rerr}");
            sysfatal(self.argv0, &m);
        }
        self.wbuf.clear();
    }
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // runes, not -1
fn rune(c: i64) -> u32 {
    c as u32
}

#[allow(clippy::struct_excessive_bools)] // the flags
struct Tr {
    cflag: bool,
    sflag: bool,
    f: Bits,
    t: Bits,
}

fn delete(tr: &mut Tr, io: &mut Io<'_>, pfrom: &mut Pcb, pto: &mut Pcb) {
    let argv0 = io.argv0;
    if tr.cflag {
        tr.f = Bits::new(0xff);
        loop {
            let c = pfrom.canon(argv0);
            if c < 0 {
                break;
            }
            tr.f.clear(rune(c));
        }
    } else {
        loop {
            let c = pfrom.canon(argv0);
            if c < 0 {
                break;
            }
            tr.f.set(rune(c));
        }
    }
    if tr.sflag {
        loop {
            let c = pto.canon(argv0);
            if c < 0 {
                break;
            }
            tr.t.set(rune(c));
        }
    }
    let mut last = NONE;
    while let Some(c) = io.readrune() {
        if !tr.f.get(c) && (c != last || !tr.t.get(c)) {
            last = c;
            io.writerune(c);
        }
    }
    io.wflush();
}

fn complement(tr: &mut Tr, io: &mut Io<'_>, pfrom: &mut Pcb, pto: &mut Pcb) {
    let argv0 = io.argv0;
    let mut lastc = 0;
    let mut high = 0;
    loop {
        let from = pfrom.canon(argv0);
        if from < 0 {
            break;
        }
        high = high.max(from);
        tr.f.set(rune(from));
    }
    let mut to;
    loop {
        to = pto.canon(argv0);
        if to <= 0 {
            break;
        }
        high = high.max(to);
        tr.t.set(rune(to));
    }
    pto.rewind();
    let p: Vec<u32> = (0..=rune(high))
        .map(|i| {
            if tr.f.get(i) {
                return i;
            }
            to = pto.canon(argv0);
            if to < 0 {
                to = lastc;
            } else {
                lastc = to;
            }
            rune(to)
        })
        .collect();
    let to = rune(to);
    let mut lastc = NONE;
    while let Some(c) = io.readrune() {
        let from = p.get(c as usize).copied().unwrap_or(to);
        if !tr.sflag || from != lastc || !tr.t.get(from) {
            lastc = from;
            io.writerune(from);
        }
    }
    io.wflush();
}

fn translit(tr: &mut Tr, io: &mut Io<'_>, pfrom: &mut Pcb, pto: &mut Pcb) {
    let argv0 = io.argv0;
    let mut lastc = 0;
    let mut high = 0;
    loop {
        let from = pfrom.canon(argv0);
        if from < 0 {
            break;
        }
        high = high.max(from);
    }
    pfrom.rewind();
    let mut p: Vec<u32> = (0..=rune(high)).collect();
    loop {
        let from = pfrom.canon(argv0);
        if from < 0 {
            break;
        }
        let mut to = pto.canon(argv0);
        if to < 0 {
            to = lastc;
        } else {
            lastc = to;
        }
        let (from, to) = (rune(from), rune(to));
        if tr.f.get(from) && p[from as usize] != to {
            sysfatal(argv0, "ambiguous translation");
        }
        tr.f.set(from);
        p[from as usize] = to;
        tr.t.set(to);
    }
    loop {
        let to = pto.canon(argv0);
        if to < 0 {
            break;
        }
        tr.t.set(rune(to));
    }
    let mut lastc = NONE;
    while let Some(c) = io.readrune() {
        let from = p.get(c as usize).copied().unwrap_or(c);
        if !tr.sflag || from != lastc || !tr.t.get(from) {
            lastc = from;
            io.writerune(from);
        }
    }
    io.wflush();
}

#[allow(clippy::unnecessary_wraps)] // the signature `main!` wants
fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(cmdline.iter());
    let argv0 = o.argv0();
    let usage = || -> ! {
        r::eprintln!("usage: {} [-cds] [string1 [string2]]", Text(argv0));
        r::exits(Some(b"usage"))
    };
    let mut tr = Tr {
        cflag: false,
        sflag: false,
        f: Bits::new(0),
        t: Bits::new(0),
    };
    let mut dflag = false;
    while let Some(c) = o.next() {
        match c {
            's' => tr.sflag = true,
            'd' => dflag = true,
            'c' => tr.cflag = true,
            _ => usage(),
        }
    }
    let argv = o.rest();
    let mut pfrom = Pcb::new(argv.first().copied().unwrap_or_default());
    let mut pto = Pcb::new(argv.get(1).copied().unwrap_or_default());
    if argv.len() > 2 {
        usage();
    }
    let mut io = Io {
        argv0,
        buf: vec![0; 4096],
        i: 0,
        n: 0,
        wbuf: Vec::with_capacity(4096),
    };
    if dflag {
        if (tr.sflag && argv.len() != 2) || (!tr.sflag && argv.len() != 1) {
            usage();
        }
        delete(&mut tr, &mut io, &mut pfrom, &mut pto);
    } else {
        if argv.len() != 2 {
            usage();
        }
        if tr.cflag {
            complement(&mut tr, &mut io, &mut pfrom, &mut pto);
        } else {
            translit(&mut tr, &mut io, &mut pfrom, &mut pto);
        }
    }
    Ok(())
}
