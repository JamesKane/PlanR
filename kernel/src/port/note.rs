//! Notes, Plan 9's software interrupts: text posted to a process, by
//! another (`#p/n/note`), by the clock (`alarm`) or by the kernel itself
//! (`sys: trap: …`), and delivered to its handler on the way back to user
//! mode ([`notify`]), or else the end of it.
//!
//! **Posting** ([`post`]) queues the note, at most [`NNOTE`] of them, and
//! interrupts the process: a sleep it is in that may be cut short
//! (`proc::Rendez::sleep_intr`) returns "interrupted", and the system
//! call with it. A note from the kernel to a process that cannot take it
//! (no handler, or one running) first throws the queue away, so it is
//! the next delivered.
//!
//! **Delivery.** The architecture's way out to user mode, from a system
//! call or a trap, calls [`notify`] while notes wait. The first goes to
//! the handler with the interrupted registers saved on the user stack
//! (`abi::note`); with no handler, or one already running, the process
//! exits with the note as its status. Notes from processes wait while a
//! handler runs; `noted` ([`noted`]) ends it and lets the next through.
//!
//! **Killing** (`#p/n/ctl` `kill`) is not a note: the process exits with
//! status `killed` on its way out, handler or not.

use alloc::sync::Arc;
use core::mem::size_of;

use abi::{ERRMAX, NOTE_GAP, NoteHow, Ureg};

use crate::arch;
use crate::port::error::{EBADARG, Error};
use crate::port::proc::{self, Proc};
use crate::port::uaccess::{self, USER_TOP};

/// The most notes queued for a process; more are dropped.
pub(crate) const NNOTE: usize = 5;

/// Who posted a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum From {
    /// A process (or the clock): held off while a handler runs.
    User,
    /// The kernel, about the process's own doing (`sys: …`).
    Kernel,
}

/// A note: up to `ERRMAX - 1` bytes of text.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Note {
    text: [u8; ERRMAX],
    len: usize,
    from: From,
}

impl Note {
    pub(crate) fn new(text: &[u8], from: From) -> Note {
        let len = text.len().min(ERRMAX - 1);
        let mut t = [0; ERRMAX];
        t[..len].copy_from_slice(&text[..len]);
        Note { text: t, len, from }
    }

    pub(crate) fn text(&self) -> &[u8] {
        &self.text[..self.len]
    }
}

/// A process's notes and its handler. Held only to look and change, with
/// no allocation, so the clock's process may post.
#[derive(Debug)]
pub(crate) struct Notes {
    queue: [Option<Note>; NNOTE],
    n: usize,
    /// The handler's address, or 0.
    handler: u64,
    /// A handler is running (Plan 9's `notified`).
    notified: bool,
    /// The note last delivered, which `noted(NDFLT)` exits with.
    last: Option<Note>,
    /// The running handler's `Ureg` on the user stack (`noteureg`).
    ureg: u64,
    /// `kill`ed: exits on the way out.
    killed: bool,
}

impl Notes {
    pub(crate) const fn new() -> Notes {
        Notes {
            queue: [None; NNOTE],
            n: 0,
            handler: 0,
            notified: false,
            last: None,
            ureg: 0,
            killed: false,
        }
    }

    /// A child's, as `rfork` makes it: the same handler, nothing queued.
    pub(crate) fn inherit(&self) -> Notes {
        Notes {
            handler: self.handler,
            ..Notes::new()
        }
    }

    /// What `exec` leaves: no handler, none running; the queue stays.
    pub(crate) fn exec(&mut self) {
        self.handler = 0;
        self.notified = false;
        self.ureg = 0;
    }

    /// Something for [`notify`] to do.
    pub(crate) fn waiting(&self) -> bool {
        self.n > 0 || self.killed
    }

    fn push(&mut self, note: Note) -> bool {
        if note.from == From::Kernel && (self.handler == 0 || self.notified) {
            self.n = 0;
        }
        if self.n == NNOTE {
            return false;
        }
        self.queue[self.n] = Some(note);
        self.n += 1;
        true
    }

    fn pop(&mut self) -> Option<Note> {
        if self.n == 0 {
            return None;
        }
        let first = self.queue[0].take();
        self.queue.copy_within(1..self.n, 0);
        self.n -= 1;
        self.queue[self.n] = None;
        first
    }
}

/// Posts `text` to `p`; false if it cannot take it (a kernel thread, or
/// its queue is full).
pub(crate) fn post(p: &Arc<Proc>, text: &[u8], from: From) -> bool {
    if !p.is_user() {
        return false;
    }
    if !p.notes().lock().push(Note::new(text, from)) {
        return false;
    }
    p.interrupt();
    true
}

/// Posts `text` to every user process in note group `noteid` but the
/// caller.
pub(crate) fn post_group(noteid: u64, text: &[u8]) {
    let me = proc::current().map(|p| p.pid());
    for p in proc::all() {
        if p.noteid() == noteid && Some(p.pid()) != me {
            post(&p, text, From::User);
        }
    }
}

/// Makes `p` exit on its way out to user mode, handler or not.
pub(crate) fn kill(p: &Arc<Proc>) -> bool {
    if !p.is_user() {
        return false;
    }
    p.notes().lock().killed = true;
    p.interrupt();
    true
}

/// `notify(handler)`: sets the calling process's handler; 0 is none.
pub(crate) fn set_handler(handler: u64) -> Result<(), Error> {
    if handler != 0 {
        uaccess::check(handler, 1)?;
    }
    let me = proc::current().ok_or(EBADARG)?;
    me.notes().lock().handler = handler;
    Ok(())
}

/// What becomes of the calling process on its way out.
enum Next {
    /// Nothing to deliver.
    Return,
    /// The note goes to the handler at the address.
    Handle(Note, u64, u64),
    /// It exits with the status.
    Exit(Note),
}

/// Delivers the calling process's next note, if one is waiting and not
/// held off: `u` (the registers it returns to user mode with) becomes a
/// call of the handler, and true is returned; with no handler the
/// process exits. Called on the way out to user mode, with interrupts on
/// and nothing held.
pub(crate) fn notify(u: &mut Ureg) -> bool {
    let Some(me) = proc::current() else {
        return false;
    };
    let next = {
        let mut n = me.notes().lock();
        me.clear_interrupted();
        if n.killed {
            Next::Exit(Note::new(b"killed", From::User))
        } else if n.n == 0 || n.notified && n.queue[0].is_some_and(|q| q.from == From::User) {
            Next::Return
        } else {
            let note = n.pop().expect("a note waits");
            n.last = Some(note);
            if n.handler == 0 || n.notified {
                Next::Exit(note)
            } else {
                n.notified = true;
                Next::Handle(note, n.handler, n.ureg)
            }
        }
    };
    let (note, handler, link) = match next {
        Next::Return => {
            me.notes_checked();
            return false;
        }
        Next::Exit(note) => {
            drop(me);
            die(note)
        }
        Next::Handle(note, handler, link) => (note, handler, link),
    };
    if let Some(ureg) = push_frame(u, &note, handler, link) {
        me.notes().lock().ureg = ureg;
        me.notes_checked();
        true
    } else {
        drop(me);
        die(note)
    }
}

/// Saves `u`, the link to the handler frame below it (`link`) and the
/// note on the user stack, and makes `u` call `handler`; returns where
/// the saved registers are.
fn push_frame(u: &mut Ureg, note: &Note, handler: u64, link: u64) -> Option<u64> {
    let ureg = u.sp().checked_sub(NOTE_GAP + size_of::<Ureg>() as u64)? & !15;
    let text = ureg.checked_sub(8 + ERRMAX as u64)?;
    let below = text.checked_sub(16)? & !15;
    uaccess::check(below, usize::try_from(ureg - below).ok()? + size_of::<Ureg>()).ok()?;
    uaccess::copy_out(ureg, bytes(u)).ok()?;
    uaccess::copy_out(ureg - 8, &link.to_ne_bytes()).ok()?;
    let mut t = [0u8; ERRMAX];
    t[..note.len].copy_from_slice(note.text());
    uaccess::copy_out(text, &t).ok()?;
    let sp = u.call(handler, below, [ureg, text]);
    uaccess::copy_out(sp, &0u64.to_ne_bytes()).ok()?;
    Some(ureg)
}

/// `noted(how)`: ends the running handler as `how` says, replacing `u`
/// with the registers it saved (or exits).
pub(crate) fn noted(how: u64, u: &mut Ureg) -> Result<(), Error> {
    let how = NoteHow::from_raw(how).ok_or(EBADARG)?;
    let me = proc::current().ok_or(EBADARG)?;
    let (notified, ureg, last) = {
        let n = me.notes().lock();
        (n.notified, n.ureg, n.last)
    };
    if !notified && how != NoteHow::Rstr {
        return Err(EBADARG.into());
    }
    let last = last.unwrap_or_else(|| Note::new(b"", From::User));
    if how == NoteHow::Dflt {
        drop(me);
        die(last);
    }
    let suicide = Note::new(b"suicide: bad ureg in noted", From::Kernel);
    let mut saved = Ureg::default();
    let mut link = [0u8; 8];
    if ureg == 0
        || uaccess::copy_in(bytes_mut(&mut saved), ureg).is_err()
        || uaccess::copy_in(&mut link, ureg - 8).is_err()
        || saved.pc() >= USER_TOP
        || saved.sp() >= USER_TOP
    {
        drop(me);
        die(suicide);
    }
    arch::context::sanitize(&mut saved, u);
    if how == NoteHow::Save {
        // The saved registers stay on the stack, for the code resumed.
        let sp = ureg.saturating_sub(8 + ERRMAX as u64 + 32) & !15;
        let pc = saved.pc();
        let sp = saved.call(pc, sp, [ureg, 0]);
        if uaccess::copy_out(sp, &0u64.to_ne_bytes()).is_err() {
            drop(me);
            die(suicide);
        }
    }
    *u = saved;
    let mut n = me.notes().lock();
    if how != NoteHow::Save {
        n.ureg = u64::from_ne_bytes(link);
    }
    n.notified = false;
    Ok(())
}

/// Exits the calling process with `note` as its status, saying so on the
/// console if the kernel posted it. Nothing may be left on the stack that
/// needs dropping.
fn die(note: Note) -> ! {
    if note.from == From::Kernel
        && let Some(me) = proc::current()
    {
        crate::println!(
            "{} {}: {}",
            me.pid(),
            me.name(),
            core::str::from_utf8(note.text()).unwrap_or("?")
        );
    }
    proc::exits(note.text().to_vec())
}

fn bytes(u: &Ureg) -> &[u8] {
    // SAFETY: `Ureg` is plain words, with no padding.
    unsafe { core::slice::from_raw_parts((&raw const *u).cast::<u8>(), size_of::<Ureg>()) }
}

fn bytes_mut(u: &mut Ureg) -> &mut [u8] {
    // SAFETY: as for `bytes`; any bytes are a valid `Ureg`.
    unsafe { core::slice::from_raw_parts_mut((&raw mut *u).cast::<u8>(), size_of::<Ureg>()) }
}
