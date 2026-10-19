//! `strings [-m min] [file ...]`: the runs of at least `min` (6)
//! printable runes in each file, or standard input, each with its
//! offset; a run of 69 is printed and a new one started, marked `...`
//! (Plan 9's `strings.c`).
//!
//! As Plan 9's: a run that starts at offset 0 is reported at the offset
//! of its second rune; files are named only when there are more than
//! two, and the names are printed straight away, ahead of the buffered
//! strings.

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
    c != r::utf::RUNE_ERROR && ((' '..'\u{7f}').contains(&c) || c > '\u{a0}')
}

fn stringit(name: &[u8], minspan: usize, out: &mut Bout<File>) {
    let mode = OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    };
    let fd = match r::cstr(name).and_then(|c| r::open(&c, mode)) {
        Ok(fd) => fd,
        Err(e) => {
            r::eprintln!("open: {e}");
            return;
        }
    };
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
            if cnt >= minspan {
                let _ = out.print(format_args!("{start:8}: {run}\n"));
            }
            start = 0;
            cnt = 0;
            run.clear();
        }
        posn = fin.offset();
    }
    if cnt >= minspan {
        let _ = out.print(format_args!("{start:8}: {run}\n"));
    }
    let _ = r::close(fd);
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let argv0 = o.argv0();
    let usage = || {
        r::eprintln!("usage: {} [-m min] [file...]", Text(argv0));
        Err("usage")
    };
    let mut minspan = MINSPAN;
    while let Some(c) = o.next() {
        match c {
            'm' => {
                let Some(m) = o.arg() else { return usage() };
                // A negative minimum is as good as none.
                minspan = usize::try_from(r::num::atol(m)).unwrap_or(0);
            }
            _ => return usage(),
        }
    }
    let files = o.rest();
    let mut out = Bout::new(r::STDOUT);
    if files.is_empty() {
        stringit(b"/fd/0", minspan, &mut out);
    }
    for f in &files {
        if files.len() > 2 {
            r::print!("{}:\n", Text(f));
        }
        stringit(f, minspan, &mut out);
    }
    out.flush().map_err(|_| "write error")
}
