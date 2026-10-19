//! `cmp [-lLs] file1 file2 [offset1 [offset2]]`: compares two files
//! byte by byte, from the offsets (in C's bases) if given, and says where
//! they first differ: with `-L`, at which line of `file1` too; with
//! `-l`, every byte that differs, in decimal position and hex; with
//! `-s`, nothing, only the exit status. A file that ends before the
//! other is reported (Plan 9's `cmp.c`, whose loop this keeps, so `-L`
//! counts lines as it does).

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec;

use r::{Access, OpenFlags, OpenMode, Text, Whence};

r::main!(run);

const BUF: usize = 65536;

fn usage() -> ! {
    r::print!("usage: cmp [-lLs] file1 file2 [offset1 [offset2] ]\n");
    r::exits(Some(b"usage"))
}

/// Seeks `fd` to the offset `arg`, if there is one.
fn seekoff(fd: r::Fd, name: &[u8], arg: Option<&[u8]>, sflag: bool) {
    let Some(a) = arg else { return };
    if !a.first().is_some_and(u8::is_ascii_digit) {
        usage();
    }
    let o = r::num::strtol(a, 0).0;
    if let Err(e) = r::seek(fd, o, Whence::Start) {
        if !sflag {
            r::eprintln!("cmp: {}: seek by {o}: {e}", Text(name));
        }
        r::exits(Some(b"seek"));
    }
}

/// One side: its buffer, and the unread part `[s, e)`; `e < s` after a
/// read error.
struct Side {
    fd: r::Fd,
    buf: alloc::vec::Vec<u8>,
    s: usize,
    e: isize,
}

impl Side {
    fn fill(&mut self) {
        #[allow(clippy::cast_possible_wrap)] // within BUF
        if self.s as isize >= self.e {
            if self.s >= BUF {
                self.s = 0;
            }
            let n = match r::read(self.fd, &mut self.buf[self.s..]) {
                Ok(n) => n as isize,
                Err(_) => -1,
            };
            self.e = self.s as isize + n;
        }
    }

    #[allow(clippy::cast_possible_wrap)] // within BUF
    fn left(&self) -> isize {
        self.e - self.s as isize
    }
}

#[allow(clippy::too_many_lines, clippy::many_single_char_names, clippy::naive_bytecount)] // Plan 9's main, and no crates
fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let (mut sflag, mut lflag, mut big_l) = (false, false, false);
    while let Some(c) = o.next() {
        match c {
            's' => sflag = true,
            'l' => lflag = true,
            'L' => big_l = true,
            _ => usage(),
        }
    }
    let rest = o.rest();
    if rest.len() < 2 || rest.len() > 4 {
        usage();
    }
    let mode = OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    };
    let (name1, name2) = (rest[0], rest[1]);
    let open = |name: &[u8]| match r::cstr(name).and_then(|c| r::open(&c, mode)) {
        Ok(fd) => fd,
        Err(e) => {
            if !sflag {
                r::eprintln!("{}: {e}", Text(name));
            }
            r::exits(Some(b"open"))
        }
    };
    let (f1, f2) = (open(name1), open(name2));
    seekoff(f1, name1, rest.get(2).copied(), sflag);
    seekoff(f2, name2, rest.get(3).copied(), sflag);
    let mut a = Side {
        fd: f1,
        buf: vec![0; BUF],
        s: 0,
        e: 0,
    };
    let mut b = Side {
        fd: f2,
        buf: vec![0; BUF],
        s: 0,
        e: 0,
    };
    let (mut nc, mut l) = (1i64, 1i64);
    loop {
        a.fill();
        b.fill();
        let n = b.left().min(a.left());
        if n <= 0 {
            break;
        }
        #[allow(clippy::cast_sign_loss)] // positive
        let n = n as usize;
        let (p, q) = (&a.buf[a.s..a.s + n], &b.buf[b.s..b.s + n]);
        if p != q {
            if sflag {
                return Err("differ");
            }
            for (i, (&x, &y)) in p.iter().zip(q).enumerate() {
                if x == b'\n' {
                    l += 1;
                }
                if x != y {
                    #[allow(clippy::cast_possible_wrap)] // within BUF
                    let at = nc + i as i64;
                    if !lflag {
                        if big_l {
                            r::print!("{} {} differ: char {at} line {l}\n", Text(name1), Text(name2));
                        } else {
                            r::print!("{} {} differ: char {at}\n", Text(name1), Text(name2));
                        }
                        return Err("differ");
                    }
                    r::print!("{at:6} 0x{x:02x} 0x{y:02x}\n");
                }
            }
        }
        if big_l {
            #[allow(clippy::cast_sign_loss)] // after a good read
            let end = a.e as usize;
            #[allow(clippy::cast_possible_wrap)] // within BUF
            {
                l += a.buf[a.s..end].iter().filter(|&&c| c == b'\n').count() as i64;
            }
        }
        #[allow(clippy::cast_possible_wrap)] // within BUF
        {
            nc += n as i64;
        }
        a.s += n;
        b.s += n;
    }
    if a.left() < 0 || b.left() < 0 {
        if !sflag {
            if a.left() < 0 {
                r::print!("error on {} after {} bytes\n", Text(name1), nc - 1);
            }
            if b.left() < 0 {
                r::print!("error on {} after {} bytes\n", Text(name2), nc - 1);
            }
        }
        return Err("read error");
    }
    if a.left() == b.left() {
        return Ok(());
    }
    if !sflag {
        let short = if a.left() > b.left() { name2 } else { name1 };
        r::print!("EOF on {} after {} bytes\n", Text(short), nc - 1);
    }
    Err("EOF")
}
