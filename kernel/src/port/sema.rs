//! User semaphores: `semacquire`, `tsemacquire` and `semrelease` on a
//! count in user memory, a 64-bit word read as signed.
//!
//! The count lives in the user's page, which the kernel changes through
//! the direct map with atomic operations (`Memory::with_word`), so no
//! user access happens under a lock. Waiters sleep on a [`Rendez`] named
//! by the word: its segment and address, which every memory sharing the
//! segment agrees on, whatever frame the page holds. A release wakes all
//! of a word's waiters, and each tries to take one under the rendez's
//! lock; those that lose sleep again.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::port::error::{EBADARG, EINTR, Error, Result};
use crate::port::lock::SpinLock;
use crate::port::proc::{self, Rendez, Woke};
use crate::port::segment::Memory;

/// Where a word's waiters sleep, while any do.
#[derive(Debug)]
struct Sema {
    seg: usize,
    va: u64,
    /// The waiters holding it.
    users: usize,
    r: Arc<Rendez>,
}

static SEMAS: SpinLock<Vec<Sema>> = SpinLock::new(Vec::new());

/// The rendez for the word `va` of segment `seg`, made if no one waits.
fn get(seg: usize, va: u64) -> Arc<Rendez> {
    let mut all = SEMAS.lock();
    if let Some(s) = all.iter_mut().find(|s| s.seg == seg && s.va == va) {
        s.users += 1;
        return s.r.clone();
    }
    let r = Arc::new(Rendez::new());
    all.push(Sema {
        seg,
        va,
        users: 1,
        r: r.clone(),
    });
    r
}

fn put(seg: usize, va: u64) {
    let mut all = SEMAS.lock();
    if let Some(i) = all.iter().position(|s| s.seg == seg && s.va == va) {
        all[i].users -= 1;
        if all[i].users == 0 {
            all.swap_remove(i);
        }
    }
}

/// Takes one from a positive count; false if it is not positive.
fn take(w: &AtomicU64) -> bool {
    w.try_update(Ordering::AcqRel, Ordering::Acquire, |v| {
        #[allow(clippy::cast_possible_wrap)] // a signed count
        ((v as i64) > 0).then(|| v - 1)
    })
    .is_ok()
}

fn memory() -> Result<Arc<Memory>> {
    proc::current().and_then(|p| p.memory()).ok_or_else(|| EBADARG.into())
}

/// Takes one from the count at `va`: at once if it is positive, else, if
/// `block`, once a release makes it so, or not at all if `deadline` comes
/// first (false). A note ends the wait with "interrupted".
pub(crate) fn acquire(va: u64, block: bool, deadline: Option<u64>) -> Result<bool> {
    let mem = memory()?;
    let s = proc::sleepable().expect("a system call may sleep");
    loop {
        let (got, seg) = mem.with_word(va, take)?;
        if got {
            return Ok(true);
        }
        if !block {
            return Ok(false);
        }
        let r = get(seg, va);
        // 1: taken; 2: the page changed under us, so try again outside.
        let state = Cell::new(0u8);
        let cond = || {
            match mem.peek_word(va, take) {
                Some(true) => state.set(1),
                Some(false) => return false,
                None => state.set(2),
            }
            true
        };
        let woke = match deadline {
            Some(d) => r.tsleep(&s, cond, d, true),
            None => match r.sleep_intr(&s, cond) {
                Ok(()) => Woke::Cond,
                Err(_) => Woke::Interrupted,
            },
        };
        put(seg, va);
        match woke {
            Woke::Cond if state.get() == 1 => return Ok(true),
            Woke::Cond => {}
            Woke::Timeout => return Ok(false),
            Woke::Interrupted => return Err(Error::from(EINTR)),
        }
    }
}

/// Adds `count` to the count at `va` and wakes its waiters; returns the
/// new count.
pub(crate) fn release(va: u64, count: u64) -> Result<u64> {
    #[allow(clippy::cast_possible_wrap)] // a signed count
    if (count as i64) < 0 {
        return Err(EBADARG.into());
    }
    let mem = memory()?;
    let (v, seg) = mem.with_word(va, |w| w.fetch_add(count, Ordering::AcqRel).wrapping_add(count))?;
    let r = SEMAS
        .lock()
        .iter()
        .find(|s| s.seg == seg && s.va == va)
        .map(|s| s.r.clone());
    if let Some(r) = r {
        r.wakeup_all();
    }
    Ok(v)
}
