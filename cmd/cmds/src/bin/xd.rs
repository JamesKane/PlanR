//! `xd [-u] [-r] [-s] [-a{odx}] [-c|-R|{b1w2l4v8}{odx}] ... [file ...]`:
//! the bytes of each file, or standard input, 16 to a line after their
//! address, in each format asked for, one line each: 1, 2, 4 (the
//! default) or 8 bytes at a time (`b w l v`, or `1 2 4 8`) in octal,
//! decimal or hex (the default); or as characters (`-c`), or runes
//! (`-R`). `-a` sets the address's base, `-r` prints `*` for lines that
//! repeat, `-s` (`-S`) swaps the bytes of each 4 (8), `-u` writes each
//! line as it is made.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use r::{Bin, Bout, File, Text};

r::main!(run);

/// The most formats.
const NARG: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Ascii,
    Rune,
}

/// A format.
#[derive(Clone, Copy)]
struct Arg {
    kind: Kind,
    /// 1, 2, 4 or 8 bytes: 0 to 3.
    loglen: usize,
    /// Octal, decimal, hex: 0 to 2.
    base: usize,
    /// The address: zero-padded (the first format's) or not, and its base.
    azero: bool,
    abase: usize,
}

/// The digits each size of number takes, in each base.
const WIDTHS: [[usize; 3]; 4] = [[3, 3, 2], [6, 5, 4], [11, 10, 8], [22, 20, 16]];

#[allow(clippy::struct_excessive_bools)] // the flags
struct Xd {
    args: alloc::vec::Vec<Arg>,
    abase: usize,
    repeats: bool,
    swizzle: bool,
    swizzle8: bool,
    flush: bool,
    out: Bout<File>,
    /// The 32 bytes read; the first 16 are shown.
    data: [u8; 32],
    odata: [u8; 16],
    ndata: usize,
    nread: usize,
    /// Where `-R` starts in the next line, after a rune that ran over.
    nstart: usize,
}

fn usage() -> ! {
    r::eprintln!("usage: xd [-u] [-r] [-s] [-a{{odx}}] [-c|{{b1w2l4v8}}{{odx}}] ... file ...");
    r::exits(Some(b"usage"))
}

fn number(v: u64, base: usize, width: usize, zero: bool) -> alloc::string::String {
    match (base, zero) {
        (0, true) => alloc::format!("{v:0width$o}"),
        (1, true) => alloc::format!("{v:0width$}"),
        (_, true) => alloc::format!("{v:0width$x}"),
        (0, false) => alloc::format!("{v:>width$o}"),
        (1, false) => alloc::format!("{v:>width$}"),
        (_, false) => alloc::format!("{v:>width$x}"),
    }
}

/// Reads until `buf` is full or the file ends, as `Bread`.
fn bread(b: &mut Bin<File>, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match b.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    n
}

impl Xd {
    fn new_arg(&mut self) -> &mut Arg {
        if self.args.len() + 1 >= NARG {
            r::eprintln!("xd: too many formats (max {NARG})");
            r::exits(Some(b"usage"));
        }
        let first = self.args.is_empty();
        self.args.push(Arg {
            kind: Kind::Number,
            loglen: 2,
            base: 2,
            azero: first,
            abase: self.abase,
        });
        self.args.last_mut().unwrap_or_else(|| unreachable!())
    }

    fn put(&mut self, s: &[u8]) {
        if self.out.write(s).is_err() {
            r::eprintln!("xd: i/o error");
            r::exits(Some(b"i/o error"));
        }
    }

    fn spaces(&mut self, n: usize) {
        // As `%*c` of a blank: at least the one.
        let s = alloc::vec![b' '; n.max(1)];
        self.put(&s);
    }

    fn address(&mut self, addr: u64, zero: bool, abase: usize) {
        let mut s = number(addr, abase, 7, zero);
        s.push(' ');
        self.put(s.as_bytes());
    }

    fn numbers(&mut self, a: Arg) {
        let size = 1 << a.loglen;
        let width = WIDTHS[a.loglen][a.base];
        let mut i = 0;
        while i < self.ndata {
            let v = self.data[i..i + size].iter().fold(0u64, |v, &b| v << 8 | u64::from(b));
            let mut s = alloc::string::String::from(" ");
            s.push_str(&number(v, a.base, width, true));
            self.put(s.as_bytes());
            i += size;
        }
    }

    fn onefmtc(&mut self, c: u8) {
        let s = match c {
            b'\t' => alloc::string::String::from(" \\t"),
            b'\r' => alloc::string::String::from(" \\r"),
            b'\n' => alloc::string::String::from(" \\n"),
            8 => alloc::string::String::from(" \\b"),
            0x20..0x7f => alloc::format!("  {}", char::from(c)),
            _ => alloc::format!(" {c:02x}"),
        };
        self.put(s.as_bytes());
    }

    fn runes(&mut self) {
        if self.nstart > 0 {
            self.spaces(3 * self.nstart);
        }
        let mut i = self.nstart;
        while i < self.ndata {
            let byte = self.data[i];
            if byte < r::utf::RUNE_SELF {
                self.onefmtc(byte);
                i += 1;
                continue;
            }
            let (rune, len) = r::utf::decode(&self.data[i..self.nread]);
            if len <= 1 || i + len > self.nread {
                self.onefmtc(byte);
                i += 1;
                continue;
            }
            // The columns the rune's bytes in this line take.
            let shown = if i + len > self.ndata { self.ndata - i } else { len };
            let text = alloc::format!("  {rune}");
            self.put(text.as_bytes());
            self.spaces(3 * shown - 3);
            i += len;
        }
        self.nstart = i.saturating_sub(self.ndata);
    }

    fn swizz(&mut self, n: usize) {
        for chunk in self.data[..16].chunks_mut(n) {
            chunk.reverse();
        }
    }

    fn end_line(&mut self) {
        self.put(b"\n");
        if self.flush {
            let _ = self.out.flush();
        }
    }

    /// Dumps file `name`, or standard input; false if it cannot be opened.
    fn xd(&mut self, name: Option<&[u8]>, title: bool) -> bool {
        let fd = match name {
            None => 0,
            Some(n) => {
                let mode = r::OpenMode {
                    access: r::Access::Read,
                    flags: r::OpenFlags::empty(),
                };
                let Ok(fd) = r::cstr(n).and_then(|c| r::open(&c, mode)) else {
                    r::eprintln!("xd: can't open {}", Text(n));
                    return false;
                };
                fd
            }
        };
        let mut bin = Bin::new(File(fd));
        if title {
            let line = [name.unwrap_or_default(), b"\n"].concat();
            self.put(&line);
        }
        let nsee = 16;
        let (mut addr, mut star, mut nleft) = (0u64, false, 0);
        loop {
            let mut data = self.data;
            let got = bread(&mut bin, &mut data[nleft..32]);
            self.data = data;
            self.ndata = got + nleft;
            nleft = 0;
            self.nread = self.ndata;
            if self.ndata > nsee {
                self.ndata = nsee;
            } else if self.ndata < nsee {
                self.data[self.ndata..nsee].fill(0);
            }
            if self.swizzle {
                self.swizz(4);
            }
            if self.swizzle8 {
                self.swizz(8);
            }
            if self.ndata == nsee && self.repeats {
                if addr > 0 && self.data[..nsee] == self.odata {
                    addr += nsee as u64;
                    if !star {
                        star = true;
                        self.put(b"*\n");
                    }
                    // As Plan 9's, this drops the bytes read past the
                    // line, so -r can lose some.
                    continue;
                }
                self.odata.copy_from_slice(&self.data[..nsee]);
                star = false;
            }
            for k in 0..self.args.len() {
                let a = self.args[k];
                self.address(addr, a.azero, a.abase);
                match a.kind {
                    Kind::Number => self.numbers(a),
                    Kind::Ascii => {
                        for i in 0..self.ndata {
                            self.onefmtc(self.data[i]);
                        }
                    }
                    Kind::Rune => self.runes(),
                }
                self.end_line();
            }
            addr += self.ndata as u64;
            if self.ndata < nsee {
                self.address(addr, true, self.abase);
                self.end_line();
                break;
            }
            if self.nread > nsee {
                nleft = self.nread - nsee;
                self.data.copy_within(nsee..self.nread, 0);
            }
        }
        if name.is_some() {
            let _ = r::close(fd);
        }
        true
    }
}

fn run(argv: r::Args) -> Result<(), &'static str> {
    let argv: alloc::vec::Vec<&[u8]> = argv.iter().collect();
    let mut x = Xd {
        args: alloc::vec::Vec::new(),
        abase: 2,
        repeats: false,
        swizzle: false,
        swizzle8: false,
        flush: false,
        out: Bout::new(r::STDOUT),
        data: [0; 32],
        odata: [0; 16],
        ndata: 0,
        nread: 0,
        nstart: 0,
    };
    let mut i = 1;
    while let Some(&a) = argv.get(i) {
        if a.len() < 2 || a[0] != b'-' {
            break;
        }
        i += 1;
        let flag = &a[1..];
        match flag {
            b"r" => x.repeats = true,
            b"s" => x.swizzle = true,
            b"S" => x.swizzle8 = true,
            b"u" => x.flush = true,
            [b'r' | b's' | b'S' | b'u', ..] => usage(),
            [b'a', rest @ ..] => {
                x.abase = match rest {
                    b"o" => 0,
                    b"d" => 1,
                    b"x" => 2,
                    _ => usage(),
                };
            }
            _ => {
                let lone = flag.len() == 1;
                let ap = x.new_arg();
                for &c in flag {
                    match c {
                        b'c' | b'R' if lone => {
                            ap.kind = if c == b'c' { Kind::Ascii } else { Kind::Rune };
                            ap.loglen = 0;
                        }
                        b'o' => ap.base = 0,
                        b'd' => ap.base = 1,
                        b'x' => ap.base = 2,
                        b'b' | b'1' => ap.loglen = 0,
                        b'w' | b'2' => ap.loglen = 1,
                        b'l' | b'4' => ap.loglen = 2,
                        b'v' | b'8' => ap.loglen = 3,
                        _ => usage(),
                    }
                }
                let abase = x.abase;
                if let Some(ap) = x.args.last_mut() {
                    ap.abase = abase;
                }
            }
        }
    }
    if x.args.is_empty() {
        x.new_arg();
    }
    let files = &argv[i..];
    let ok = match files {
        [] => x.xd(None, false),
        [f] => x.xd(Some(f), false),
        _ => {
            let mut ok = true;
            for f in files {
                ok &= x.xd(Some(f), true);
            }
            ok
        }
    };
    let _ = x.out.flush();
    if ok { Ok(()) } else { Err("error") }
}
