//! `wc [-lwrbc] [file ...]`: counts the lines, words, runes, broken
//! UTF-8 sequences ("bad runes") and bytes of each file, or of standard
//! input, and the total of several; lines, words and bytes unless told
//! (Plan 9's `wc.c`). Words are separated by white space, as
//! `isspacerune` sees it; a bad rune is a rune but not part of a word.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::string::String;
use core::fmt::Write as _;

use r::utf::{RUNE_ERROR, is_space_rune};
use r::{Access, Bin, File, OpenFlags, OpenMode};

r::main!(run);

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    lines: u64,
    words: u64,
    runes: u64,
    bad: u64,
    bytes: u64,
}

impl core::ops::AddAssign for Counts {
    fn add_assign(&mut self, o: Counts) {
        self.lines += o.lines;
        self.words += o.words;
        self.runes += o.runes;
        self.bad += o.bad;
        self.bytes += o.bytes;
    }
}

fn wc(bin: &mut Bin<File>) -> Counts {
    let mut c = Counts::default();
    let mut in_word = false;
    while let Ok(Some(r)) = bin.getrune() {
        c.runes += 1;
        if r == RUNE_ERROR {
            c.bad += 1;
            continue;
        }
        if r == '\n' {
            c.lines += 1;
        }
        if in_word {
            in_word = !is_space_rune(r);
        } else if !is_space_rune(r) {
            in_word = true;
            c.words += 1;
        }
    }
    c.bytes = bin.offset();
    c
}

/// Which counts to print.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // the flags
struct Show {
    lines: bool,
    words: bool,
    runes: bool,
    bad: bool,
    bytes: bool,
}

fn report(show: &Show, c: &Counts, name: Option<&[u8]>) {
    let mut line = String::new();
    for (on, n) in [
        (show.lines, c.lines),
        (show.words, c.words),
        (show.runes, c.runes),
        (show.bad, c.bad),
        (show.bytes, c.bytes),
    ] {
        if on {
            let _ = write!(line, " {n:7}");
        }
    }
    let mut out = line.into_bytes();
    if let Some(name) = name {
        out.push(b' ');
        out.extend_from_slice(name);
    }
    out.push(b'\n');
    let _ = r::write(1, out.get(1..).unwrap_or_default());
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut show = Show::default();
    while let Some(c) = o.next() {
        match c {
            'l' => show.lines = true,
            'w' => show.words = true,
            'r' => show.runes = true,
            'b' => show.bad = true,
            'c' => show.bytes = true,
            _ => {
                r::eprintln!("Usage: {} [-lwrbc] [file ...]", r::Text(o.argv0()));
                return Err("usage");
            }
        }
    }
    if !(show.lines || show.words || show.runes || show.bad || show.bytes) {
        show.lines = true;
        show.words = true;
        show.bytes = true;
    }
    let files = o.rest();
    if files.is_empty() {
        report(&show, &wc(&mut Bin::new(r::STDIN)), None);
        return Ok(());
    }
    let mode = OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    };
    let mut status = Ok(());
    let mut total = Counts::default();
    for f in &files {
        match r::cstr(f).and_then(|c| r::open(&c, mode)) {
            Ok(fd) => {
                let c = wc(&mut Bin::new(File(fd)));
                report(&show, &c, Some(f));
                total += c;
                let _ = r::close(fd);
            }
            Err(e) => {
                r::eprintln!("{}: {e}", r::Text(f));
                status = Err("can't open");
            }
        }
    }
    if files.len() > 1 {
        report(&show, &total, Some(b"total"));
    }
    status
}
