//! Descriptors: the shell redirects its own, so builtins and functions
//! see the redirections too, and puts them back after. A descriptor about
//! to be redirected is first saved as a copy at 10 or above; the shell's
//! own descriptors (those copies and the input it reads commands from)
//! are closed in every program it runs, and moved out of the way of a
//! redirection that names them.
//!
//! The shell's own messages, though, go to [`err`]: what descriptor 2 was
//! when it started. Plan 9's rc makes redirections only for the programs
//! it runs (and `whatis` looks its output up through them), so its
//! messages, its prompts and `-x`'s lines never follow one.
//!
//! Also the inputs commands are read from: a descriptor ([`FdInput`]),
//! seen through a [`Source`] that prompts and echoes.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::sync::atomic::Ordering;

use r::{Access, Fd, OpenFlags, OpenMode};
use rc::lex::Input;

use crate::flag;

/// Where saved copies start.
const FIRST: Fd = 10;

/// The shell's standard error, for its own messages: see the module.
static ERR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(2);

/// Where the shell's own messages go.
pub(crate) fn err() -> Fd {
    ERR.load(Ordering::Relaxed)
}

/// A descriptor the shell keeps for itself, which may move.
pub(crate) type Private = Rc<Cell<Fd>>;

/// How to put a descriptor back.
#[derive(Debug)]
pub(crate) struct Saved {
    fd: Fd,
    /// The copy of what `fd` was, if it was open.
    copy: Option<Private>,
}

#[derive(Debug, Default)]
pub(crate) struct Fds {
    private: Vec<Private>,
    /// The copy of the first descriptor 2, which [`err`] gives.
    err: Option<Private>,
    /// The descriptors redirected now, which a copy must not take.
    redirected: Vec<Fd>,
    /// Descriptors kept open for programs to use, which a copy must not
    /// take either.
    held: Vec<Fd>,
}

impl Fds {
    /// Keeps a copy of descriptor 2 for the shell's own messages.
    pub(crate) fn keep_err(&mut self) {
        if let Ok(fd) = r::dup(2, Some(self.free(2))) {
            let p = self.adopt(fd);
            ERR.store(p.get(), Ordering::Relaxed);
            self.err = Some(p);
        }
    }

    /// Takes `fd` as the shell's own; it may be moved later.
    pub(crate) fn adopt(&mut self, fd: Fd) -> Private {
        let p = Rc::new(Cell::new(fd));
        self.private.push(p.clone());
        self.moved_from(fd);
        p
    }

    /// Stops keeping `p`, and closes it.
    pub(crate) fn release(&mut self, p: &Private) {
        self.private.retain(|q| !Rc::ptr_eq(p, q));
        let _ = r::close(p.get());
    }

    /// Stops keeping `p` as the shell's own, but keeps it open, where it
    /// is, for the programs the shell runs; returns it.
    pub(crate) fn keep(&mut self, p: &Private) -> Fd {
        self.private.retain(|q| !Rc::ptr_eq(p, q));
        self.held.push(p.get());
        p.get()
    }

    /// Closes `fd`, which [`Fds::keep`] kept.
    pub(crate) fn forget(&mut self, fd: Fd) {
        self.held.retain(|&h| h != fd);
        let _ = r::close(fd);
    }

    /// The lowest descriptor from [`FIRST`], other than `not`, that
    /// nothing of the shell's uses.
    fn free(&self, not: Fd) -> Fd {
        (FIRST..FIRST + 1000)
            .find(|&n| {
                n != not
                    && !self.private.iter().any(|p| p.get() == n)
                    && !self.redirected.contains(&n)
                    && !self.held.contains(&n)
            })
            .unwrap_or(FIRST)
    }

    /// Moves any private descriptor at `fd` elsewhere, so `fd` can be
    /// used.
    fn moved_from(&mut self, fd: Fd) {
        let Some(p) = self.private.iter().find(|p| p.get() == fd).cloned() else {
            return;
        };
        let to = self.free(fd);
        if r::dup(fd, Some(to)).is_ok() {
            let _ = r::close(fd);
            p.set(to);
            if self.err.as_ref().is_some_and(|e| Rc::ptr_eq(e, &p)) {
                ERR.store(to, Ordering::Relaxed);
            }
        }
    }

    /// Saves what `fd` is now, before it is redirected.
    pub(crate) fn save(&mut self, fd: Fd) -> Saved {
        self.moved_from(fd);
        let to = self.free(fd);
        let copy = r::dup(fd, Some(to)).ok().map(|c| {
            let p = Rc::new(Cell::new(c));
            self.private.push(p.clone());
            p
        });
        self.redirected.push(fd);
        Saved { fd, copy }
    }

    /// Puts back what `s` saved.
    pub(crate) fn restore(&mut self, s: Saved) {
        if let Some(i) = self.redirected.iter().rposition(|&f| f == s.fd) {
            self.redirected.remove(i);
        }
        match s.copy {
            Some(c) => {
                let _ = r::dup(c.get(), Some(s.fd));
                self.release(&c);
            }
            None => {
                let _ = r::close(s.fd);
            }
        }
    }

    /// Closes the shell's own descriptors, in a process about to run a
    /// program.
    pub(crate) fn close_private(&self) {
        for p in &self.private {
            let _ = r::close(p.get());
        }
    }
}

pub(crate) const fn mode(access: Access, flags: OpenFlags) -> OpenMode {
    OpenMode { access, flags }
}

/// Commands read from a descriptor, a buffer at a time.
#[derive(Debug)]
pub(crate) struct FdInput {
    fd: Private,
    buf: Vec<u8>,
    at: usize,
    end: bool,
}

impl FdInput {
    pub(crate) fn new(fd: Private) -> FdInput {
        FdInput {
            fd,
            buf: Vec::new(),
            at: 0,
            end: false,
        }
    }
}

impl rc::lex::Input for FdInput {
    fn next(&mut self) -> Option<u8> {
        if self.at == self.buf.len() {
            if self.end {
                return None;
            }
            self.buf.resize(8192, 0);
            match r::read(self.fd.get(), &mut self.buf) {
                Ok(n) if n > 0 => {
                    self.buf.truncate(n);
                    self.at = 0;
                }
                // Not the end: an interactive shell reads on.
                Err(e) if e.as_bytes() == b"interrupted" => {
                    self.buf.clear();
                    self.at = 0;
                    return None;
                }
                _ => {
                    self.buf.clear();
                    self.at = 0;
                    self.end = true;
                    return None;
                }
            }
        }
        let c = self.buf[self.at];
        self.at += 1;
        Some(c)
    }
}

/// What the shell's inputs share with it: the prompts of an interactive
/// one, for a command's first line and for the lines after, and the
/// input echoed for `-v` and `-V`, not yet written. That waits, as in
/// Plan 9's buffered standard error, until the shell next reads a
/// command, prompts, reports an error or runs a program
/// ([`Echo::flush`]).
#[derive(Debug, Default)]
pub(crate) struct Echo {
    pub(crate) first: Vec<u8>,
    pub(crate) rest: Vec<u8>,
    echoed: Vec<u8>,
}

impl Echo {
    pub(crate) fn flush(&mut self) {
        if !self.echoed.is_empty() {
            let _ = r::io::Write::write_all(&mut r::File(err()), &self.echoed);
            self.echoed.clear();
        }
    }
}

/// An input as the lexer reads it: prompting before each line if it is
/// interactive, and echoing what it reads with `-V`, or with `-v` past
/// rcmain.
pub(crate) struct Source {
    input: Box<dyn Input>,
    echo: Rc<RefCell<Echo>>,
    interactive: bool,
    /// A line starts with the next byte.
    at_line: bool,
}

impl Source {
    pub(crate) fn new(input: Box<dyn Input>, echo: Rc<RefCell<Echo>>, interactive: bool) -> Source {
        Source {
            input,
            echo,
            interactive,
            at_line: true,
        }
    }
}

impl Input for Source {
    fn next(&mut self) -> Option<u8> {
        let mut e = self.echo.borrow_mut();
        if self.at_line && self.interactive {
            let rest = e.rest.clone();
            let p = core::mem::replace(&mut e.first, rest);
            e.echoed.extend_from_slice(&p);
            e.flush();
        }
        let c = self.input.next();
        self.at_line = matches!(c, None | Some(b'\n'));
        if let Some(c) = c
            && (flag::get(b'V') || flag::get(b'v') && flag::NDOT.load(Ordering::Relaxed) >= 2)
        {
            e.echoed.push(c);
        }
        c
    }
}
