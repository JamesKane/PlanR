//! `rc`, the shell: Plan 9's rc, rewritten.
//!
//! ```text
//! rc [-c command] [file [arg ...]]
//! ```
//!
//! It reads commands from `command`, else from `file` (with `$0` set to
//! its name), else from standard input, with `$*` set to the arguments
//! after them, and exits with the last command's `$status`. Variables and
//! functions come from `/env` and go back to it for the programs it
//! runs.
//!
//! The syntax is in the `rc` library (`lex`, `parse`, `tree`, `pattern`);
//! running it is here: [`exec`] the commands, [`word`] the words,
//! [`var`] the variables, [`fd`] descriptors and redirections, and
//! [`builtin`] the builtins.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

mod builtin;
mod exec;
mod fd;
mod var;
mod word;

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;

use rc::parse::Parser;
use rc::tree::Cmd;

use crate::exec::Shell;
use crate::fd::{FdInput, mode};

r::main!(run);

fn usage() -> ! {
    r::eprintln!("usage: rc [-c command] [file [arg ...]]");
    r::exits(Some(b"usage"))
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut command = None;
    while let Some(c) = o.next() {
        match c {
            'c' => command = Some(o.arg().unwrap_or_else(|| usage())),
            _ => usage(),
        }
    }
    let argv0 = o.argv0().to_vec();
    let mut rest = o.rest().into_iter().map(<[u8]>::to_vec);

    let mut sh = Shell::default();
    sh.argv0.clone_from(&argv0);
    import(&mut sh);
    if let Ok(pid) = r::getpid() {
        sh.vars.set(b"pid", alloc::vec![format!("{pid}").into_bytes()]);
    }
    if sh.vars.get(b"path").is_empty() {
        sh.vars.set(b"path", alloc::vec![b".".to_vec(), b"/bin".to_vec()]);
    }
    if sh.vars.get(b"ifs").is_empty() {
        sh.vars.set(b"ifs", alloc::vec![b" \t\n".to_vec()]);
    }
    sh.set_status(b"");

    let res = if let Some(cmd) = command {
        sh.vars.set(b"*", rest.collect());
        sh.vars.set(b"0", alloc::vec![argv0]);
        let mut text = cmd.to_vec();
        text.push(b'\n');
        sh.eval_text(&text)
    } else if let Some(file) = rest.next() {
        sh.vars.set(b"*", rest.collect());
        sh.vars.set(b"0", alloc::vec![file.clone()]);
        match r::cstr(&file).and_then(|f| r::open(&f, mode(r::Access::Read, r::OpenFlags::empty()))) {
            Ok(fd) => {
                let fd = sh.fds.adopt(fd);
                sh.run_input(Box::new(FdInput::new(fd)), Some(&file), false)
            }
            Err(e) => {
                r::eprintln!("rc: can't open {}: {e}", r::print::Text(&file));
                sh.set_status(b"can't open");
                Ok(())
            }
        }
    } else {
        sh.vars.set(b"*", Vec::new());
        sh.vars.set(b"0", alloc::vec![argv0]);
        let stdin = Rc::new(Cell::new(0));
        sh.run_input(Box::new(FdInput::new(stdin)), None, false)
    };
    let _ = res;
    sh.exit()
}

/// Takes the variables and functions in `/env`.
fn import(sh: &mut Shell) {
    let Ok(fd) = r::open(c"/env", mode(r::Access::Read, r::OpenFlags::empty())) else {
        return;
    };
    let entries = r::dirreadall(fd).unwrap_or_default();
    let _ = r::close(fd);
    for e in entries {
        let name = e.name.as_bytes();
        if let Some(f) = name.strip_prefix(b"fn#") {
            if let Some(body) = r::env::getenv_raw(name).and_then(|t| function(&t)) {
                sh.vars.import_func(f, body);
            }
        } else if let Some(val) = r::env::getenv_list(name) {
            sh.vars.import(name, val);
        }
    }
}

/// The body of the function that `fn name {body}` defines.
fn function(text: &[u8]) -> Option<Rc<Cmd>> {
    let mut p = Parser::new(Box::new(rc::lex::Text::new(text)));
    match p.line() {
        Ok(Some(Cmd::Fn(_, Some(body)))) => Some(Rc::new(*body)),
        _ => None,
    }
}
