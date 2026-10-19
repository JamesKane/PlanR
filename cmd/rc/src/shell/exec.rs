//! Running the tree: the shell's state and what each command does.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::ffi::CString;
use alloc::format;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::ffi::CStr;

use r::{Access, OpenFlags, RforkFlags as F, Whence};
use rc::lex::{Input, RedirKind, SyntaxError};
use rc::parse::Parser;
use rc::tree::{Arg, Cmd, Redir, Word};

use crate::builtin;
use crate::fd::{Fds, Saved, mode};
use crate::var::{List, Vars};

/// Why a command stopped short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unwind {
    /// An error, already reported, with `$status` set: it ends every
    /// command up to the nearest interactive loop, or else the shell.
    Error,
    /// `exit`: the shell ends with `$status`.
    Exit,
}

pub(crate) type Run<T = ()> = Result<T, Unwind>;

#[derive(Debug, Default)]
pub(crate) struct Shell {
    pub(crate) vars: Vars,
    pub(crate) fds: Fds,
    /// Whether the last `if` did not run its command, so `if not` should.
    ifnot: bool,
    /// The children of `&` not yet waited for.
    pub(crate) apids: Vec<u64>,
    /// Children a pipeline waits for, and the status of those that ended
    /// while something else was waited for.
    pending: Vec<u64>,
    reaped: BTreeMap<u64, Vec<u8>>,
    /// How the shell was named, for its error messages.
    pub(crate) argv0: Vec<u8>,
}

impl Shell {
    /// `$status`'s first word.
    pub(crate) fn status(&self) -> Vec<u8> {
        self.vars.get(b"status").first().cloned().unwrap_or_default()
    }

    pub(crate) fn set_status(&mut self, s: &[u8]) {
        self.vars.set(b"status", alloc::vec![s.to_vec()]);
    }

    /// Whether `$status` is true: empty, or nothing but zeros and bars.
    pub(crate) fn true_status(&self) -> bool {
        self.status().iter().all(|&c| c == b'0' || c == b'|')
    }

    /// Prints `rc: msg` (naming the shell if it is not plain `rc`) and
    /// unwinds, as Plan 9's `Xerror1`.
    pub(crate) fn error<T>(&mut self, msg: &str) -> Run<T> {
        let who = r::print::Text(&self.argv0);
        if self.argv0 == b"rc" || self.argv0 == b"/bin/rc" {
            r::eprintln!("rc: {msg}");
        } else {
            r::eprintln!("rc ({who}): {msg}");
        }
        self.set_status(b"error");
        Err(Unwind::Error)
    }

    /// [`Shell::error`] with the system's error text, as `Xerror`.
    pub(crate) fn error_r<T>(&mut self, msg: &str, e: &r::Error) -> Run<T> {
        self.error(&format!("{msg}: {e}"))
    }

    /// Ends the shell with `$status`.
    pub(crate) fn exit(&mut self) -> ! {
        let s = self.status();
        r::exits(if s.is_empty() { None } else { Some(&s) })
    }

    /// Reads commands from `input` a line at a time and runs each. A
    /// syntax error ends the input, unless the shell is interactive; an
    /// error ends it too, and unwinds further.
    pub(crate) fn run_input(&mut self, input: Box<dyn Input>, name: Option<&[u8]>, interactive: bool) -> Run {
        let mut p = Parser::new(input);
        loop {
            match p.line() {
                Ok(None) => return Ok(()),
                Ok(Some(c)) => match self.run(&c, false) {
                    Err(Unwind::Error) if interactive => {}
                    r => r?,
                },
                Err(e) => {
                    self.syntax_error(&e, name, interactive);
                    if !interactive {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn syntax_error(&mut self, e: &SyntaxError, name: Option<&[u8]>, interactive: bool) {
        let mut msg = Vec::from(&b"rc: "[..]);
        match name {
            Some(n) if !interactive => msg.extend(format!("{}:{}: ", r::print::Text(n), e.line).bytes()),
            Some(n) => msg.extend(format!("{}: ", r::print::Text(n)).bytes()),
            None if !interactive => msg.extend(format!("line {}: ", e.line).bytes()),
            None => {}
        }
        if !e.token.is_empty() {
            msg.extend(format!("token {}: ", r::RcQuoted(&e.token)).bytes());
        }
        msg.extend_from_slice(e.msg.as_bytes());
        msg.push(b'\n');
        let _ = r::io::Write::write_all(&mut r::File(2), &msg);
        self.set_status(e.msg.as_bytes());
    }

    /// Runs `c`. `tail` says nothing follows it in this process, which is
    /// a child that will exit after: a program it runs replaces it.
    pub(crate) fn run(&mut self, c: &Cmd, tail: bool) -> Run {
        match c {
            Cmd::Empty => Ok(()),
            Cmd::Simple(args) => self.simple(args, tail),
            Cmd::Brace(c) => self.run(c, tail),
            Cmd::Seq(a, b) => {
                self.run(a, false)?;
                self.run(b, tail)
            }
            Cmd::Async(c) => self.background(c),
            Cmd::And(a, b) => {
                self.run(a, false)?;
                if self.true_status() {
                    self.run(b, tail)?;
                }
                Ok(())
            }
            Cmd::Or(a, b) => {
                self.run(a, false)?;
                if !self.true_status() {
                    self.run(b, tail)?;
                }
                Ok(())
            }
            Cmd::Pipe(left, lfd, rfd, right) => self.pipe(left, *lfd, *rfd, right),
            Cmd::Not(c) => {
                self.run(c, false)?;
                let s: &[u8] = if self.true_status() { b"false" } else { b"" };
                self.set_status(s);
                Ok(())
            }
            Cmd::Subshell(c) => {
                let pid = self.fork(F::PROC | F::FDG | F::REND | F::ENVG)?;
                if pid == 0 {
                    self.child(c);
                }
                self.wait_for(pid)
            }
            Cmd::Redir(r, c) => {
                let s = self.redirect(r)?;
                let res = self.run(c, tail);
                self.fds.restore(s);
                res
            }
            Cmd::Assign(name, value, c) => {
                let name = self.name(name)?;
                let value = self.glob_words(core::slice::from_ref(value))?;
                if **c == Cmd::Empty {
                    self.vars.set(&name, value);
                    return Ok(());
                }
                self.vars.push_local(&name, value);
                let res = self.run(c, false);
                self.vars.pop_local();
                res
            }
            Cmd::If(cond, c) => {
                self.run(cond, false)?;
                self.ifnot = true;
                if self.true_status() {
                    self.run(c, false)?;
                    self.ifnot = false;
                }
                Ok(())
            }
            Cmd::IfNot(c) => {
                if self.ifnot {
                    self.run(c, false)?;
                }
                Ok(())
            }
            Cmd::For(name, words, c) => self.for_loop(name, words.as_deref(), c),
            Cmd::While(cond, c) => loop {
                self.run(cond, false)?;
                if **cond == Cmd::Empty {
                    self.set_status(b"");
                }
                if !self.true_status() {
                    return Ok(());
                }
                self.run(c, false)?;
            },
            Cmd::Switch(w, body) => self.switch(w, body),
            Cmd::Match(w, pats) => self.matching(w, pats),
            Cmd::Fn(names, body) => {
                let body = body.as_ref().map(|b| Rc::new((**b).clone()));
                for n in self.plain(names)? {
                    self.vars.set_func(&n, body.clone());
                }
                Ok(())
            }
        }
    }

    /// `~ subject patterns`: the subject's words, joined by blanks,
    /// against each pattern.
    fn matching(&mut self, w: &Word, pats: &[Word]) -> Run {
        let subject = self.plain(core::slice::from_ref(w))?.join(&b' ');
        let pats = self.eval_words(pats)?;
        let s: &[u8] = if pats.iter().any(|p| p.matches(&subject)) {
            b""
        } else {
            b"no match"
        };
        self.set_status(s);
        Ok(())
    }

    fn for_loop(&mut self, name: &Word, words: Option<&[Word]>, c: &Cmd) -> Run {
        let list = match words {
            Some(ws) => self.glob_words(ws)?,
            None => self.vars.get(b"*").clone(),
        };
        let name = self.name(name)?;
        self.vars.push_local(&name, List::new());
        let mut res = Ok(());
        for v in list {
            self.vars.set(&name, alloc::vec![v]);
            res = self.run(c, false);
            if res.is_err() {
                break;
            }
        }
        self.vars.pop_local();
        res
    }

    /// `switch`: the commands after the first `case` that matches, up to
    /// the next `case`.
    fn switch(&mut self, w: &Word, body: &Cmd) -> Run {
        let mut cmds = Vec::new();
        let mut c = body;
        while let Cmd::Seq(a, b) = c {
            cmds.push(&**a);
            c = b;
        }
        cmds.push(c);
        if !cmds[0].is_case() {
            return self.error("case missing in switch");
        }
        let subject = self.plain(core::slice::from_ref(w))?.join(&b' ');
        let mut matched = false;
        for c in cmds {
            if let (true, Cmd::Simple(args)) = (c.is_case(), c) {
                if matched {
                    break;
                }
                let words: Vec<Word> = args[1..]
                    .iter()
                    .filter_map(|a| match a {
                        Arg::Word(w) => Some(w.clone()),
                        Arg::Redir(_) => None,
                    })
                    .collect();
                matched = self.eval_words(&words)?.iter().any(|p| p.matches(&subject));
            } else if matched {
                self.run(c, false)?;
            }
        }
        Ok(())
    }

    /// A simple command: its redirections, in order, then its words.
    fn simple(&mut self, args: &[Arg], tail: bool) -> Run {
        let mut saved = Vec::new();
        let mut res = Ok(());
        for a in args {
            if let Arg::Redir(r) = a {
                match self.redirect(r) {
                    Ok(s) => saved.push(s),
                    Err(e) => {
                        res = Err(e);
                        break;
                    }
                }
            }
        }
        if res.is_ok() {
            let words: Vec<Word> = args
                .iter()
                .filter_map(|a| match a {
                    Arg::Word(w) => Some(w.clone()),
                    Arg::Redir(_) => None,
                })
                .collect();
            res = self.glob_words(&words).and_then(|argv| self.command(argv, tail));
        }
        for s in saved.into_iter().rev() {
            self.fds.restore(s);
        }
        res
    }

    /// Runs `argv`: a function, a builtin, or a program.
    pub(crate) fn command(&mut self, mut argv: List, tail: bool) -> Run {
        if argv.is_empty() {
            return self.error("empty argument list");
        }
        if let Some(f) = self.vars.func(&argv[0]) {
            let rest = argv.split_off(1);
            return self.call(&f, rest);
        }
        if argv[0] == b"builtin" {
            if argv.len() == 1 {
                r::eprintln!("builtin: empty argument list");
                self.set_status(b"empty arg list");
                return Ok(());
            }
            argv.remove(0);
        }
        if let Some(b) = builtin::find(&argv[0]) {
            return b(self, &argv);
        }
        self.vars.export();
        if tail {
            self.exec(&argv);
        }
        let pid = self.fork(F::PROC | F::FDG | F::REND)?;
        if pid == 0 {
            self.exec(&argv);
        }
        self.wait_for(pid)
    }

    /// Calls function `f` with `$*` set to `args`.
    pub(crate) fn call(&mut self, f: &Cmd, args: List) -> Run {
        self.vars.push_local(b"*", args);
        let res = self.run(f, false);
        self.vars.pop_local();
        res
    }

    /// Replaces the shell with program `argv`, found as `$path` says; if
    /// that fails, says why and exits.
    pub(crate) fn exec(&mut self, argv: &[Vec<u8>]) -> ! {
        self.fds.close_private();
        let strings: Result<Vec<CString>, _> = argv.iter().map(|a| r::cstr(a)).collect();
        let e = match strings {
            Ok(strings) => {
                let refs: Vec<&CStr> = strings.iter().map(CString::as_c_str).collect();
                r::exec_path(&argv[0], &refs)
            }
            Err(e) => e,
        };
        r::eprintln!("{}: {e}", r::print::Text(&argv[0]));
        r::exits(Some(e.as_bytes()))
    }

    /// Forks: 0 in the child.
    pub(crate) fn fork(&mut self, flags: F) -> Run<u64> {
        match r::rfork(flags) {
            Ok(pid) => Ok(pid),
            Err(e) => self.error_r("try again", &e),
        }
    }

    /// In a child the shell forked, runs `c` and exits.
    pub(crate) fn child(&mut self, c: &Cmd) -> ! {
        self.apids.clear();
        self.pending.clear();
        self.reaped.clear();
        let _ = self.run(c, true);
        self.exit()
    }

    /// Waits for child `pid` and sets `$status` from how it ended. Others
    /// that end meanwhile are kept for whoever waits for them.
    pub(crate) fn wait_for(&mut self, pid: u64) -> Run {
        if let Some(s) = self.reaped.remove(&pid) {
            self.set_status(&s);
            return Ok(());
        }
        loop {
            let Ok(w) = r::wait() else {
                return Ok(());
            };
            self.apids.retain(|&p| p != w.pid);
            if w.pid == pid {
                self.set_status(&w.msg);
                return Ok(());
            }
            if self.pending.contains(&w.pid) {
                self.reaped.insert(w.pid, w.msg);
            }
        }
    }

    /// `cmd &`: in a child with its own note group, reading `/dev/null`.
    fn background(&mut self, c: &Cmd) -> Run {
        let null = match r::open(c"/dev/null", mode(Access::Read, OpenFlags::empty())) {
            Ok(fd) => fd,
            Err(e) => return self.error_r("can't open /dev/null", &e),
        };
        let pid = match self.fork(F::PROC | F::FDG | F::NOTEG | F::ENVG) {
            Ok(pid) => pid,
            Err(e) => {
                let _ = r::close(null);
                return Err(e);
            }
        };
        if pid == 0 {
            move_fd(null, 0);
            self.child(c);
        }
        let _ = r::close(null);
        self.apids.push(pid);
        self.vars.set(b"apid", alloc::vec![format!("{pid}").into_bytes()]);
        Ok(())
    }

    /// `left |[lfd=rfd] right`: `left` in a child writing descriptor `lfd`
    /// into the pipe, `right` here reading it on `rfd`. The status is
    /// both, `left`'s first.
    fn pipe(&mut self, left: &Cmd, lfd: u32, rfd: u32, right: &Cmd) -> Run {
        let [wr, rd] = match r::pipe() {
            Ok(p) => p,
            Err(e) => return self.error_r("can't get pipe", &e),
        };
        let (wr, rd) = (self.fds.adopt(wr), self.fds.adopt(rd));
        let pid = match self.fork(F::PROC | F::FDG | F::REND | F::ENVG) {
            Ok(pid) => pid,
            Err(e) => {
                self.fds.release(&wr);
                self.fds.release(&rd);
                return Err(e);
            }
        };
        if pid == 0 {
            self.fds.release(&rd);
            // Redirected until the child exits.
            let _ = self.fds.save(lfd as usize);
            let _ = r::dup(wr.get(), Some(lfd as usize));
            self.fds.release(&wr);
            self.child(left);
        }
        self.fds.release(&wr);
        self.pending.push(pid);
        let saved = self.fds.save(rfd as usize);
        let _ = r::dup(rd.get(), Some(rfd as usize));
        self.fds.release(&rd);
        let res = self.run(right, false);
        self.fds.restore(saved);
        let right = self.status();
        let waited = self.wait_for(pid);
        self.pending.retain(|&p| p != pid);
        res?;
        waited?;
        let mut st = self.status();
        st.push(b'|');
        st.extend_from_slice(&right);
        self.set_status(&st);
        Ok(())
    }

    /// Makes redirection `r`, saving what it replaces.
    fn redirect(&mut self, r: &Redir) -> Run<Saved> {
        match r {
            Redir::File(kind, fd, w) => {
                let name = self.glob_words(core::slice::from_ref(w))?;
                let [name] = &name[..] else {
                    let what = if *kind == RedirKind::Read { "<" } else { ">" };
                    return self.error(&format!("{what} requires singleton"));
                };
                let fd = *fd as usize;
                let s = self.fds.save(fd);
                match open_for(*kind, name) {
                    Ok(f) => {
                        move_fd(f, fd);
                        Ok(s)
                    }
                    Err(e) => {
                        self.fds.restore(s);
                        r::eprint!("{}: ", r::print::Text(name));
                        self.error_r("can't open", &e)
                    }
                }
            }
            Redir::Dup(fd, from) => {
                let s = self.fds.save(*fd as usize);
                let _ = r::dup(*from as usize, Some(*fd as usize));
                Ok(s)
            }
            Redir::Close(fd) => {
                let s = self.fds.save(*fd as usize);
                let _ = r::close(*fd as usize);
                Ok(s)
            }
        }
    }

    /// Runs the text `cmds` as commands, as `eval` does.
    pub(crate) fn eval_text(&mut self, cmds: &[u8]) -> Run {
        let input = Box::new(rc::lex::Text::new(cmds));
        self.run_input(input, None, false)
    }
}

/// Opens `name` for a redirection of `kind`.
fn open_for(kind: RedirKind, name: &[u8]) -> Result<r::Fd, r::Error> {
    let name = r::cstr(name)?;
    let write = mode(Access::Write, OpenFlags::empty());
    match kind {
        RedirKind::Read | RedirKind::Here => r::open(&name, mode(Access::Read, OpenFlags::empty())),
        RedirKind::ReadWrite => r::open(&name, mode(Access::ReadWrite, OpenFlags::empty())),
        RedirKind::Write => r::create(&name, write, 0o666),
        RedirKind::Append => {
            let fd = r::open(&name, write).or_else(|_| r::create(&name, write, 0o666))?;
            let _ = r::seek(fd, 0, Whence::End);
            Ok(fd)
        }
    }
}

/// Makes `to` what `from` is, and closes `from`.
pub(crate) fn move_fd(from: r::Fd, to: r::Fd) {
    if from != to {
        let _ = r::dup(from, Some(to));
        let _ = r::close(from);
    }
}
