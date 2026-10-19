//! Notes, as Plan 9's rc takes them: the handler only counts the ones it
//! knows (an interrupted system call then fails, and a wait for a program
//! carries on), and the shell acts on them between commands
//! ([`Shell::dotrap`]): by calling the function named for the note
//! (`sigint` and so on), else for `interrupt` and `quit` by unwinding to
//! the nearest interactive loop, else by exiting. Any other note is
//! reported and ends the shell as it would without a handler.

use core::ffi::CStr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use r::NoteHow;

use crate::exec::{Run, Shell, Unwind};

/// The notes the shell takes (each matched as a prefix) and the functions
/// named for them.
const NOTES: [(&[u8], &[u8]); 8] = [
    (b"exit", b"sigexit"),
    (b"hangup", b"sighup"),
    (b"interrupt", b"sigint"),
    (b"quit", b"sigquit"),
    (b"alarm", b"sigalrm"),
    (b"kill", b"sigkill"),
    (b"sys: fp: ", b"sigfpe"),
    (b"term", b"sigterm"),
];

const INTERRUPT: usize = 2;
const QUIT: usize = 3;

/// Notes of each kind not yet acted on, and all of them.
static TRAPS: [AtomicU32; NOTES.len()] = [const { AtomicU32::new(0) }; NOTES.len()];
static NTRAP: AtomicU32 = AtomicU32::new(0);

/// A note other than a `sys:` one came since [`clear_interrupted`].
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn init() {
    let _ = r::notify(Some(handler));
}

pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

pub(crate) fn clear_interrupted() {
    INTERRUPTED.store(false, Ordering::Relaxed);
}

extern "C" fn handler(_: *mut r::Ureg, note: *const u8) {
    // SAFETY: the kernel passes the note's NUL-terminated text.
    let text = unsafe { CStr::from_ptr(note.cast()) }.to_bytes();
    // Nothing here allocates: the note may have stopped the allocator.
    let Some(i) = NOTES.iter().position(|(n, _)| text.starts_with(n)) else {
        for part in [&b"rc: note: "[..], text, b"\n"] {
            let _ = r::write(2, part);
        }
        let _ = r::noted(NoteHow::Dflt);
        r::exits(Some(text))
    };
    if !text.starts_with(b"sys: ") {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }
    // Interrupts do not pile up.
    if text != b"interrupt" || TRAPS[i].load(Ordering::Relaxed) == 0 {
        TRAPS[i].fetch_add(1, Ordering::Relaxed);
        if NTRAP.fetch_add(1, Ordering::Relaxed) + 1 >= 32 {
            for part in [&b"rc: Too many traps (trap "[..], text, b"), aborting\n"] {
                let _ = r::write(2, part);
            }
            let _ = r::noted(NoteHow::Dflt);
            r::exits(Some(b"abort"))
        }
    }
    let _ = r::noted(NoteHow::Cont);
    r::exits(Some(b"noted failed"))
}

impl Shell {
    /// Acts on the notes that have come, as Plan 9's `dotrap`. A child the
    /// shell forked just exits.
    pub(crate) fn dotrap(&mut self) -> Run {
        while NTRAP.load(Ordering::Relaxed) > 0 {
            for (i, &(_, name)) in NOTES.iter().enumerate() {
                while TRAPS[i].load(Ordering::Relaxed) > 0 {
                    TRAPS[i].fetch_sub(1, Ordering::Relaxed);
                    NTRAP.fetch_sub(1, Ordering::Relaxed);
                    if self.forked {
                        self.exit();
                    }
                    if let Some(f) = self.vars.func(name) {
                        let star = self.vars.get(b"*").clone();
                        self.call(&f, star)?;
                    } else if i == INTERRUPT || i == QUIT {
                        return Err(Unwind::Error);
                    } else {
                        self.exit();
                    }
                }
            }
        }
        Ok(())
    }
}
