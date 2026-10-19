//! `strings [file ...]`: the runs of at least 6 printable runes in each
//! file, or standard input, each with its offset; a run of 69 is printed
//! and a new one started, marked `...`. With several files, each is
//! named first.
//!
//! As Plan 9's, a run that starts at offset 0 is reported at the offset
//! of its second rune.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::string::String;

use r::{Access, Bin, Bout, File, OpenFlags, OpenMode, Text};

r::main!(run);

const MINSPAN: usize = 6;
const BUFSIZE: usize = 70;

fn printable(c: char) -> bool {
    (' '..'\u{7f}').contains(&c) || c > '\u{a0}'
}

fn stringit(fd: r::Fd, out: &mut Bout<File>) {
    let mut fin = Bin::new(File(fd));
    let mut run = String::new();
    let mut cnt = 0;
    let mut start = 0;
    let mut posn = fin.offset();
    while let Ok(Some(c)) = fin.getrune() {
        if printable(c) {
            if start == 0 {
                start = posn;
            }
            run.push(c);
            cnt += 1;
            if cnt == BUFSIZE - 1 {
                let _ = out.print(format_args!("{start:8}: {run} ...\n"));
                start = 0;
                cnt = 0;
                run.clear();
            }
        } else {
            if cnt >= MINSPAN {
                let _ = out.print(format_args!("{start:8}: {run}\n"));
            }
            start = 0;
            cnt = 0;
            run.clear();
        }
        posn = fin.offset();
    }
    if cnt >= MINSPAN {
        let _ = out.print(format_args!("{start:8}: {run}\n"));
    }
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut out = Bout::new(r::STDOUT);
    if args.len() < 2 {
        stringit(0, &mut out);
        return out.flush().map_err(|_| "write error");
    }
    let mode = OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    };
    for f in args.iter().skip(1) {
        // Unbuffered, unlike the strings, as Plan 9's: with output to
        // a file, the names come first.
        if args.len() > 2 {
            r::print!("{}:\n", Text(f));
        }
        match r::cstr(f).and_then(|c| r::open(&c, mode)) {
            Ok(fd) => {
                stringit(fd, &mut out);
                let _ = r::close(fd);
            }
            Err(e) => r::eprintln!("open: {e}"),
        }
    }
    out.flush().map_err(|_| "write error")
}
