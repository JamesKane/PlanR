//! Regular expressions, as Plan 9's `regexp(2)` (`libregexp`): a pattern
//! is compiled ([`regcomp`]) to a program for a machine that runs every
//! way through it at once over the text, a rune at a time, and finds the
//! leftmost longest match ([`regexec`] on UTF-8, [`rregexec`] on runes),
//! with the extent of each parenthesized subexpression. [`regsub`] and
//! [`rregsub`] substitute a match into a template.
//!
//! The syntax is Plan 9's `regexp(6)`: `.` (not newline, except for
//! [`regcompnl`]), `[]` and `[^]` classes (a negated class never matches
//! newline), `^`, `$`, `*`, `+`, `?`, `|` and `()`, and `\` quoting any
//! rune. [`regcomplit`] takes every rune literally.
//!
//! A match or subexpression is a [`Resub`]: offsets into the text, in bytes
//! for [`regexec`] and in runes for [`rregexec`].

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod comp;
mod exec;
mod sub;

use alloc::vec::Vec;

pub use comp::{regcomp, regcomplit, regcompnl};
pub use exec::{regexec, rregexec};
pub use sub::{regsub, rregsub};

/// The most subexpressions a match records, the whole match included.
pub const NSUBEXP: usize = 32;

/// A match or subexpression: where it starts and ends in the text, if it
/// did. Set in the first given to [`regexec`], they also bound where it
/// looks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resub {
    pub sp: Option<usize>,
    pub ep: Option<usize>,
}

/// A class of runes: sorted, disjoint ranges, first and last.
#[derive(Debug, Clone, Default)]
struct Reclass {
    spans: Vec<[u32; 2]>,
}

// The instruction types (regcomp.h). 02xx are operators, the value their
// precedence; 03xx are operands.
const RUNE: u16 = 0o177;
const OPERATOR: u16 = 0o200;
const START: u16 = 0o200;
const RBRA: u16 = 0o201;
const LBRA: u16 = 0o202;
const OR: u16 = 0o203;
const CAT: u16 = 0o204;
const STAR: u16 = 0o205;
const PLUS: u16 = 0o206;
const QUEST: u16 = 0o207;
const ANY: u16 = 0o300;
const ANYNL: u16 = 0o301;
const NOP: u16 = 0o302;
const BOL: u16 = 0o303;
const EOL: u16 = 0o304;
const CCLASS: u16 = 0o305;
const NCCLASS: u16 = 0o306;
const END: u16 = 0o377;

/// No instruction (C's nil).
const NIL: usize = usize::MAX;

/// An instruction of the machine. C keeps `r`, `subid`, `cp` and `right`
/// in one union, and `left` and `next` in another; `next` is `left`.
#[derive(Debug, Clone, Copy)]
struct Reinst {
    ty: u16,
    r: char,
    subid: usize,
    cp: usize,
    right: usize,
    next: usize,
}

/// A compiled regular expression.
#[derive(Debug, Clone)]
pub struct Reprog {
    startinst: usize,
    class: Vec<Reclass>,
    inst: Vec<Reinst>,
}
