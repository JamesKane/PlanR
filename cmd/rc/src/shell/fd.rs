//! Descriptors: the shell redirects its own, so builtins and functions
//! see the redirections too, and puts them back after. A descriptor about
//! to be redirected is first saved as a copy at 10 or above; the shell's
//! own descriptors (those copies and the input it reads commands from)
//! are closed in every program it runs, and moved out of the way of a
//! redirection that names them.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;

use r::{Access, Fd, OpenFlags, OpenMode};

/// Where saved copies start.
const FIRST: Fd = 10;

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
    /// The descriptors redirected now, which a copy must not take.
    redirected: Vec<Fd>,
}

impl Fds {
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

    /// The lowest descriptor from [`FIRST`], other than `not`, that
    /// nothing of the shell's uses.
    fn free(&self, not: Fd) -> Fd {
        (FIRST..FIRST + 1000)
            .find(|&n| n != not && !self.private.iter().any(|p| p.get() == n) && !self.redirected.contains(&n))
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
