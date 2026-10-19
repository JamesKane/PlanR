//! rc's syntax: the lexer ([`lex`]), the parser ([`parse`]) and its tree
//! ([`tree`], printed as Plan 9 prints it), and patterns ([`pattern`]).
//! The shell itself, which runs the tree, is the `rc` program.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod lex;
pub mod parse;
pub mod pattern;
pub mod tree;
