//! Timers: things to do at a time, for timed sleeps and alarms.
//!
//! A timer readies a thread whose sleep has run out of time, or posts its
//! `alarm` note. The system's clock tick (CPU 0's) only looks at when the
//! next is due, without a lock, and wakes the `clock` kernel process,
//! which runs those that are; so posting a note, which may take locks a
//! tick must not, happens in a thread. Timers run at tick resolution
//! ([`clock::HZ`]).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::port::clock;
use crate::port::lock::SpinLock;
use crate::port::proc::{self, Proc, Rendez, Spawn};

/// What a timer does when it comes due.
#[derive(Debug, Clone, Copy)]
pub(crate) enum What {
    /// Ends the thread's sleep with this number, if it still sleeps in it.
    Wake(u64),
    /// Posts `alarm`, if the thread's alarm is still set for this time.
    Alarm,
}

#[derive(Debug)]
struct Timer {
    id: u64,
    /// When it is due, in counter nanoseconds.
    when: u64,
    proc: Weak<Proc>,
    what: What,
}

static TIMERS: SpinLock<Vec<Timer>> = SpinLock::new(Vec::new());
/// When the first timer is due; `u64::MAX` for none.
static NEXT: AtomicU64 = AtomicU64::new(u64::MAX);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Where the clock's process waits for the next to come due.
static DUE: Rendez = Rendez::new();

/// Sets a timer to do `what` for `p` at `when`; returns its number, for
/// [`cancel`].
pub(crate) fn add(when: u64, p: &Arc<Proc>, what: What) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    TIMERS.lock().push(Timer {
        id,
        when,
        proc: Arc::downgrade(p),
        what,
    });
    NEXT.fetch_min(when, Ordering::SeqCst);
    id
}

/// Takes timer `id` away, if it has not run.
pub(crate) fn cancel(id: u64) {
    let gone = {
        let mut t = TIMERS.lock();
        t.iter().position(|t| t.id == id).map(|i| t.swap_remove(i))
    };
    // The process it held may go with it: dropped outside the lock.
    drop(gone);
}

/// The system's tick: wakes the clock's process if a timer is due.
pub(crate) fn tick() {
    if NEXT.load(Ordering::SeqCst) <= clock::now_ns() {
        DUE.wakeup();
    }
}

/// Starts the clock's process.
pub(crate) fn start() {
    proc::spawn(Spawn::kproc("clock"), run).expect("no memory for the clock's process");
}

fn run() {
    let s = proc::sleepable().expect("a kernel process may sleep");
    loop {
        DUE.sleep(&s, || NEXT.load(Ordering::SeqCst) <= clock::now_ns());
        let now = clock::now_ns();
        let due: Vec<Timer> = {
            let mut t = TIMERS.lock();
            let due = t.extract_if(.., |t| t.when <= now).collect();
            NEXT.store(t.iter().map(|t| t.when).min().unwrap_or(u64::MAX), Ordering::SeqCst);
            due
        };
        for t in due {
            let Some(p) = t.proc.upgrade() else { continue };
            match t.what {
                What::Wake(seq) => p.timer_wake(seq),
                What::Alarm => p.alarm_fired(t.when),
            }
        }
    }
}
