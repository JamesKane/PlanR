//! `wc [-lwrbc] [file ...]`: counts the lines, words, runes, bytes that
//! are not UTF-8 ("bad runes") and bytes of each file, or of standard
//! input, and the total of several; lines, words and bytes unless told.
//! Words are separated by blanks, tabs and newlines.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::string::String;
use core::fmt::Write as _;

use r::{Access, OpenFlags, OpenMode};

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

/// Where in the text: between words, in one, or in a rune that needs
/// this many more bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Space,
    Word,
    More(u8),
}

/// Counts `b`: the state after it, and whether it starts a word, is not
/// a rune of its own, is bad, ends a line. This is Plan 9's table.
fn step(s: State, b: u8) -> (State, bool, bool, bool, bool) {
    use State::{More, Space, Word};
    match s {
        Space | Word => {
            let w = s == Space;
            match b {
                b'\t' | b' ' => (Space, false, false, false, false),
                b'\n' => (Space, false, false, false, true),
                0..=0x7f => (Word, w, false, false, false),
                0xc0..=0xdf => (More(1), w, false, false, false),
                0xe0..=0xef => (More(2), w, false, false, false),
                0xf0..=0xf7 => (More(3), w, false, false, false),
                _ => (Word, w, false, true, false),
            }
        }
        More(n) => match b {
            b'\t' | b' ' => (Space, false, false, true, false),
            b'\n' => (Space, false, false, true, true),
            0x80..=0x97 if n == 3 => (More(2), false, true, false, false),
            0x80..=0xbf if n < 3 => (if n == 1 { Word } else { More(1) }, false, true, false, false),
            0xc0..=0xdf => (More(1), false, false, true, false),
            0xe0..=0xef => (More(2), false, false, true, false),
            0xf0..=0xf7 => (More(3), false, false, true, false),
            _ => (Word, false, false, true, false),
        },
    }
}

fn count(fd: r::Fd, name: &str) -> Counts {
    let mut c = Counts::default();
    let mut state = State::Space;
    let mut buf = alloc::vec![0u8; 8192];
    loop {
        let n = match r::read(fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                r::eprintln!("{name}: {e}");
                break;
            }
        };
        c.bytes += n as u64;
        c.runes += n as u64;
        for &b in &buf[..n] {
            let (next, word, part, bad, line) = step(state, b);
            state = next;
            c.words += u64::from(word);
            c.runes -= u64::from(part);
            c.bad += u64::from(bad);
            c.lines += u64::from(line);
        }
    }
    if matches!(state, State::More(_)) {
        c.bad += 1;
    }
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

fn report(show: &Show, c: &Counts, name: Option<&str>) {
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
    if let Some(name) = name {
        let _ = write!(line, " {name}");
    }
    r::println!("{}", line.get(1..).unwrap_or(""));
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
        report(&show, &count(0, ""), None);
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
                let name = alloc::format!("{}", r::Text(f));
                let c = count(fd, &name);
                report(&show, &c, Some(&name));
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
        report(&show, &total, Some("total"));
    }
    status
}
