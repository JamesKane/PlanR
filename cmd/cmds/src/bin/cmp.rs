//! `cmp [-lsL] file1 file2 [offset1 [offset2]]`: compares two files
//! byte by byte, from the offsets (in C's bases) if given, and says where
//! they first differ: with `-L`, at which line of `file1` too; with
//! `-l`, every byte that differs, in decimal position and hex; with
//! `-s`, nothing, only the exit status. A file that ends before the
//! other is reported.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use r::{Access, Bin, File, OpenFlags, OpenMode, Text, Whence};

r::main!(run);

fn usage() -> Result<(), &'static str> {
    r::eprintln!("Usage: cmp [-lsL] file1 file2 [offset1 [offset2] ]");
    Err("usage")
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let (mut sflag, mut lflag, mut big_l) = (false, false, false);
    while let Some(c) = o.next() {
        match c {
            's' => sflag = true,
            'l' => lflag = true,
            'L' => big_l = true,
            _ => return usage(),
        }
    }
    let rest = o.rest();
    if rest.len() < 2 || rest.len() > 4 {
        return usage();
    }
    let mode = OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    };
    let mut fds = Vec::new();
    for &name in &rest[..2] {
        match r::cstr(name).and_then(|c| r::open(&c, mode)) {
            Ok(fd) => fds.push(fd),
            Err(e) => {
                if !sflag {
                    r::eprintln!("{}: {e}", Text(name));
                }
                return Err("open");
            }
        }
    }
    for (i, off) in rest[2..].iter().enumerate() {
        if let Err(e) = r::seek(fds[i], r::num::strtol(off, 0).0, Whence::Start) {
            if !sflag {
                r::eprintln!("cmp: seek by offset{}: {e}", i + 1);
            }
            return Err(if i == 0 { "seek 1" } else { "seek 2" });
        }
    }
    let (name1, name2) = (Text(rest[0]), Text(rest[1]));
    let mut f1 = Bin::new(File(fds[0]));
    let mut f2 = Bin::new(File(fds[1]));
    let mut out = r::Bout::new(r::STDOUT);
    let (mut nc, mut line) = (1u64, 1u64);
    loop {
        let a = f1.getc().map_err(|_| "read error")?;
        let b = f2.getc().map_err(|_| "read error")?;
        let (a, b) = match (a, b) {
            (Some(a), Some(b)) => (a, b),
            (None, None) => return out.flush().map_err(|_| "write error"),
            (_, None) | (None, _) => {
                if !sflag {
                    let short = if a.is_none() { name1 } else { name2 };
                    let _ = out.print(format_args!("EOF on {short}\n"));
                }
                let _ = out.flush();
                return Err("EOF");
            }
        };
        if a == b'\n' {
            line += 1;
        }
        if a != b {
            if sflag {
                return Err("differ");
            }
            if !lflag {
                let _ = out.print(format_args!("{name1} {name2} differ: char {nc}"));
                let _ = if big_l {
                    out.print(format_args!(" line {line}\n"))
                } else {
                    out.write(b"\n")
                };
                let _ = out.flush();
                return Err("differ");
            }
            let _ = out.print(format_args!("{nc:6} 0x{a:02x} 0x{b:02x}\n"));
        }
        nc += 1;
    }
}
