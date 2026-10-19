//! grep's pattern machine, as Plan 9's `grep` (`grep.h`, `grep.y`,
//! `comp.c`, `sub.c`): the pattern is parsed to a graph of byte
//! classes, alternations and anchors (UTF-8 classes become alternations
//! of byte sequences), and that graph is run as a DFA built as the text
//! needs it: each state is the sorted set of graph nodes the bytes so
//! far can reach, found again in a tree of the states so far. A state
//! after a newline says whether the line matched.
//!
//! The command itself, reading the files, is `main.rs` (`main.c`).
//! Where C exits, at an error in the pattern or of its own, this gives a
//! [`Fatal`].

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod comp;
mod parse;
mod sub;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// No node or state (C's nil).
pub const NIL: usize = usize::MAX;

// The kinds of node.
const TALT: u8 = 1;
const TBEGIN: u8 = 2;
const TCASE: u8 = 3;
const TCLASS: u8 = 4;
const TEND: u8 = 5;
const TOR: u8 = 6;

/// An alternation of this many bytes or more becomes a table.
const CASELIM: u32 = 7;
/// The "byte" before the text, which only `^` follows.
const CBEGIN: u32 = 0x10_ffff + 1;
/// Output is flushed every this many lines and one.
pub const FLSHCNT: i64 = (1 << 9) - 1;

pub const CFLAG: u32 = 1 << 0;
pub const HFLAG: u32 = 1 << 1;
pub const IFLAG: u32 = 1 << 2;
pub const LLFLAG: u32 = 1 << 3;
pub const LLLFLAG: u32 = 1 << 4;
pub const NFLAG: u32 = 1 << 5;
pub const SFLAG: u32 = 1 << 6;
pub const VFLAG: u32 = 1 << 7;
pub const BFLAG: u32 = 1 << 8;

/// A node of the pattern's graph. C keeps `alt`, `cases`, `lo`/`hi` in a
/// union.
#[derive(Debug, Clone)]
struct Re {
    ty: u8,
    /// When it was last visited. C's is a `ushort`, which wraps after
    /// 65536 states made; this does not.
    generation: u32,
    /// `TALT`, `TOR`: the other way.
    alt: usize,
    /// `TCASE`: the node each byte goes to.
    cases: Option<Box<[usize; 256]>>,
    /// `TCLASS`: the bytes it takes.
    lo: u32,
    hi: u32,
    next: usize,
}

/// A fragment of graph: its first node and the list (through `next`) of
/// its ends, still to be joined to what follows.
#[derive(Debug, Clone, Copy)]
struct Re2 {
    beg: usize,
    end: usize,
}

/// A state of the DFA: the nodes it is, sorted, whether a line that ends
/// in it matched, its place in the tree of states, and the state each
/// byte leads to, once known.
#[derive(Debug)]
pub struct State {
    pub matched: bool,
    re: Vec<usize>,
    linkleft: usize,
    linkright: usize,
    pub next: Box<[usize; 256]>,
}

/// Why grep stops: what it says on standard error and the status it
/// exits with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fatal {
    pub msg: String,
    pub status: String,
}

/// Where the pattern is read from: the argument, or the `-f` file.
#[derive(Debug, Default)]
struct Input {
    arg: Vec<u8>,
    at: usize,
    file: Vec<u8>,
    file_at: usize,
}

/// grep's state: C's globals.
#[derive(Debug)]
pub struct Grep {
    re: Vec<Re>,
    pub states: Vec<State>,
    follow: Vec<usize>,
    generation: u32,
    maxfollow: usize,
    matched: bool,
    pub state0: usize,
    topre: Re2,
    /// How many times each flag was given.
    pub flags: [u32; 256],
    input: Input,
    /// The line of the pattern file being read, for errors.
    pub lineno: i64,
    literal: bool,
    peekc: Option<parse::Tok>,
    /// The pattern file's name and the pattern, for errors.
    filename: Option<Vec<u8>>,
    pattern: Option<Vec<u8>>,
}

impl Default for Grep {
    fn default() -> Grep {
        Grep::new()
    }
}

impl Grep {
    #[must_use]
    pub fn new() -> Grep {
        Grep {
            re: Vec::new(),
            states: Vec::new(),
            follow: Vec::new(),
            generation: 0,
            maxfollow: 0,
            matched: false,
            state0: NIL,
            topre: Re2 { beg: NIL, end: NIL },
            flags: [0; 256],
            input: Input::default(),
            lineno: 0,
            literal: false,
            peekc: None,
            filename: None,
            pattern: None,
        }
    }

    /// Reads patterns from `contents`, the file `name` (`-f`), from now
    /// on.
    pub fn pattern_file(&mut self, name: &[u8], contents: Vec<u8>) {
        self.flags[usize::from(b'f')] += 1;
        self.filename = Some(name.to_vec());
        self.input.file = contents;
        self.input.file_at = 0;
        self.lineno = 1;
    }

    /// The state after byte `c` from `s`, made if it is new.
    pub fn step(&mut self, s: usize, c: u8) -> Result<usize, Fatal> {
        let ns = self.states[s].next[usize::from(c)];
        if ns != NIL {
            return Ok(ns);
        }
        self.increment(s, c)?;
        Ok(self.states[s].next[usize::from(c)])
    }
}

fn fatal(msg: String, status: &str) -> Fatal {
    Fatal {
        msg,
        status: status.into(),
    }
}

/// C's `error`: grep's own trouble.
fn error(s: &str) -> Fatal {
    fatal(alloc::format!("grep: internal error: {s}"), s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines of `text` that `patterns` match, with the flags given.
    fn grep(patterns: &[&str], flags: &str, text: &str) -> Result<Vec<String>, Fatal> {
        let mut g = Grep::new();
        for f in flags.bytes() {
            g.flags[usize::from(f)] += 1;
        }
        for p in patterns {
            g.str2top(p.as_bytes())?;
        }
        g.initstate()?;
        let fold = g.flags[usize::from(b'i')] > 0;
        let mut out = Vec::new();
        for line in text.split_inclusive('\n') {
            let mut s = g.state0;
            for &b in line.as_bytes() {
                let c = if fold { b.to_ascii_lowercase() } else { b };
                s = g.step(s, c)?;
            }
            if g.states[s].matched {
                out.push(line.trim_end_matches('\n').into());
            }
        }
        Ok(out)
    }

    fn lines(patterns: &[&str], text: &str) -> Vec<String> {
        grep(patterns, "", text).unwrap()
    }

    #[test]
    fn matches() {
        let text = "apple\nbanana\ncherry\nplum\n";
        assert_eq!(lines(&["an"], text), ["banana"]);
        assert_eq!(lines(&["^p"], text), ["plum"]);
        assert_eq!(lines(&["e$"], text), ["apple"]);
        assert_eq!(lines(&["a.a"], text), ["banana"]);
        assert_eq!(lines(&["(ch|pu|lu)"], text), ["cherry", "plum"]);
        assert_eq!(lines(&["p+l"], text), ["apple", "plum"]);
        assert_eq!(lines(&["rr?y"], text), ["cherry"]);
        assert_eq!(lines(&["x*"], text).len(), 4);
        assert_eq!(lines(&["^$"], "a\n\nb\n"), [""]);
        assert_eq!(lines(&["an", "um"], text), ["banana", "plum"]);
        assert_eq!(lines(&["an\num"], text), ["banana", "plum"]);
    }

    #[test]
    fn classes() {
        let text = "abc\nxyz\n123\né\nä\n";
        assert_eq!(lines(&["[a-c]"], text), ["abc"]);
        assert_eq!(lines(&["^[^a-z]+$"], text), ["123", "é", "ä"]);
        assert_eq!(lines(&["[0-9][0-9]"], text), ["123"]);
        assert_eq!(lines(&["[é]"], text), ["é"]);
        assert_eq!(lines(&["[à-é]"], text), ["é", "ä"]);
        assert_eq!(lines(&["^.$"], text), ["é", "ä"]);
        assert_eq!(lines(&["[\\]]"], "a]\nb\n"), ["a]"]);
    }

    #[test]
    fn literal_and_fold() {
        assert_eq!(lines(&["*a.c"], "abc\na.c\n"), ["a.c"]);
        assert_eq!(lines(&["a\\.c"], "abc\na.c\n"), ["a.c"]);
        assert_eq!(grep(&["ABC"], "i", "xabcx\nAbC\nab\n").unwrap(), ["xabcx", "AbC"]);
        // The tables of alternatives (`TCASE`) and long alternations.
        let words = "one|two|three|four|five|six|seven|eight";
        assert_eq!(lines(&[words], "a five\nnine\nseventy\n"), ["a five", "seventy"]);
    }

    #[test]
    fn errors() {
        let err = |p: &str| grep(&[p], "", "").unwrap_err();
        assert_eq!(err("(a").msg, "grep: (a: syntax error");
        assert_eq!(err("a)").msg, "grep: a): syntax error");
        assert_eq!(err("a|").msg, "grep: a|: syntax error");
        assert_eq!(err("(*a)").msg, "grep: (*a): syntax error");
        assert_eq!(err("a[b").msg, "grep: a[b: syntax error");
        // What yacc reduces by default at the start.
        assert_eq!(err("|a").msg, "grep: |a: empty pattern");
        assert_eq!(err("[a").msg, "grep: [a: empty pattern");
        assert_eq!(err("+").msg, "grep: +: empty pattern");
        assert_eq!(err("").msg, "grep: empty pattern");
        assert_eq!(err("").status, "syntax");
        // Plan 9's classes stop at 3-byte runes.
        assert_eq!(err("[^😀]").msg, "grep: internal error: length");
    }

    #[test]
    fn pattern_files() {
        let mut g = Grep::new();
        g.pattern_file(b"pats", b"a\n(b\n".to_vec());
        let e = g.str2top(b"pats").unwrap_err();
        assert_eq!(e.msg, "grep: pats:3: syntax error");
    }
}
