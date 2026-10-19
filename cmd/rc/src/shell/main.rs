//! `rc`, the shell: Plan 9's rc, rewritten.
//!
//! ```text
//! rc [-srdiIlxepvV] [-c command] [-m rcmain] [file [arg ...]]
//! ```
//!
//! As Plan 9's, it takes its variables from `/env`, sets `$*` to the
//! arguments after the flags, and runs `. rcmain $*`, which (from
//! `/rc/lib/rcmain`, or `-m`'s file) sets the defaults and runs
//! `command`, `file`, or standard input, interactively if `-i` is given
//! or standard input is the console. The flags: `-e` exits when a command
//! fails, `-x` prints each simple command before it runs, `-v` and `-V`
//! echo the input, `-s` prints a failed status before each command,
//! `-l` reads `$home/lib/profile` (as rcmain says), `-p` keeps rcmain
//! from taking functions from the environment or `$path` from it, `-I`
//! is never interactive; `-r` and `-d` do nothing.
//!
//! The syntax is in the `rc` library (`lex`, `parse`, `tree`, `pattern`);
//! running it is here: [`exec`] the commands, [`word`] the words,
//! [`var`] the variables, [`fd`] descriptors, redirections and inputs,
//! [`builtin`] the builtins, [`flag`] the flags and [`trap`] notes.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

mod builtin;
mod exec;
mod fd;
mod flag;
mod trap;
mod var;
mod word;

use alloc::format;
use alloc::vec::Vec;

use crate::exec::{Shell, Unwind};

r::main!(run);

/// Where rcmain is, unless `-m` says.
const RCMAIN: &[u8] = b"/rc/lib/rcmain";

fn run(args: r::Args) -> Result<(), &'static str> {
    let words: Vec<&[u8]> = args.iter().collect();
    let a = flag::parse(&words);
    let argv0 = words.first().copied().unwrap_or(b"rc").to_vec();
    if argv0.first() == Some(&b'-') {
        flag::set(b'l', true);
    }
    if flag::get(b'I') {
        flag::set(b'i', false);
    } else if !flag::get(b'i') && a.rest.is_empty() && isatty(0) {
        flag::set(b'i', true);
    }
    trap::init();

    let mut sh = Shell::default();
    sh.argv0.clone_from(&argv0);
    sh.vars.import_vars();
    if let Ok(pid) = r::getpid() {
        sh.vars.set(b"pid", alloc::vec![format!("{pid}").into_bytes()]);
    }
    sh.vars.set(b"cflag", a.command.into_iter().collect());
    sh.vars.set(b"rcname", alloc::vec![argv0]);
    sh.vars.set(b"*", a.rest.clone());
    let mut dot = alloc::vec![b".".to_vec(), a.rcmain.unwrap_or_else(|| RCMAIN.to_vec())];
    dot.extend(a.rest);
    match sh.command(dot, false) {
        Err(Unwind::Error) => sh.exit(),
        _ => sh.exit_with_sigexit(),
    }
}

/// Whether `fd` is the console.
fn isatty(fd: r::Fd) -> bool {
    let mut buf = [0u8; 256];
    r::fd2path(fd, &mut buf).is_ok_and(|n| {
        let name = &buf[..n];
        name == b"#c/cons" || name.ends_with(b"/dev/cons")
    })
}
