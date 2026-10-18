//! Interrupt dispatch: a handler per vector, and counts for reporting.
//!
//! What a vector number means is the architecture's business (an IDT
//! vector on amd64, a GIC interrupt ID on arm64). The architecture code
//! acknowledges the controller around [`dispatch`].

use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::arch;
use crate::port::lock::SpinLock;

/// An interrupt handler. Runs with interrupts off.
pub(crate) type Handler = fn();

#[derive(Debug)]
struct Slot {
    /// A [`Handler`] as a pointer, or null.
    handler: AtomicPtr<()>,
    count: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)] // only an array initializer
const EMPTY: Slot = Slot {
    handler: AtomicPtr::new(core::ptr::null_mut()),
    count: AtomicU64::new(0),
};

static SLOTS: [Slot; arch::NVEC] = [EMPTY; arch::NVEC];
static NAMES: SpinLock<[&str; arch::NVEC]> = SpinLock::new([""; arch::NVEC]);

/// Why a handler could not be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    OutOfRange(usize),
    Busy { vector: usize, owner: &'static str },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::OutOfRange(v) => write!(f, "vector {v} out of range"),
            Error::Busy { vector, owner } => write!(f, "vector {vector} already used by {owner}"),
        }
    }
}

/// Installs `handler` for `vector`. The caller enables the source.
pub(crate) fn register(vector: usize, name: &'static str, handler: Handler) -> Result<(), Error> {
    let slot = SLOTS.get(vector).ok_or(Error::OutOfRange(vector))?;
    let mut names = NAMES.lock();
    if !slot.handler.load(Ordering::Relaxed).is_null() {
        return Err(Error::Busy {
            vector,
            owner: names[vector],
        });
    }
    names[vector] = name;
    slot.handler.store(handler as *mut (), Ordering::Release);
    Ok(())
}

/// Runs the handler for `vector`; false if there is none.
pub(crate) fn dispatch(vector: usize) -> bool {
    let Some(slot) = SLOTS.get(vector) else {
        return false;
    };
    let h = slot.handler.load(Ordering::Acquire);
    if h.is_null() {
        return false;
    }
    slot.count.fetch_add(1, Ordering::Relaxed);
    // SAFETY: only `register` stores here, and it stores a `Handler`.
    let h = unsafe { core::mem::transmute::<*mut (), Handler>(h) };
    h();
    true
}

/// How many times `vector`'s handler has run.
pub(crate) fn count(vector: usize) -> u64 {
    SLOTS.get(vector).map_or(0, |s| s.count.load(Ordering::Relaxed))
}

/// Calls `f` with each registered vector, its name and its count.
pub(crate) fn each(mut f: impl FnMut(usize, &str, u64)) {
    let names = NAMES.lock();
    for (v, slot) in SLOTS.iter().enumerate() {
        if !slot.handler.load(Ordering::Relaxed).is_null() {
            f(v, names[v], slot.count.load(Ordering::Relaxed));
        }
    }
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use crate::port::ktest::TestResult;
    use core::sync::atomic::{AtomicU64, Ordering};

    use crate::arch;
    use crate::port::clock;

    static HITS: AtomicU64 = AtomicU64::new(0);

    fn hit() {
        HITS.fetch_add(1, Ordering::Relaxed);
    }

    /// A software interrupt to this CPU reaches its handler: the whole
    /// controller path, from send through acknowledge and end-of-interrupt.
    pub(crate) fn self_ipi_delivered() -> TestResult {
        static ONCE: AtomicU64 = AtomicU64::new(0);
        if ONCE.fetch_add(1, Ordering::Relaxed) == 0 {
            super::register(arch::TEST_VECTOR, "ktest ipi", hit).map_err(|_| "vector busy")?;
        }
        for round in 1..=3 {
            arch::self_ipi(arch::TEST_VECTOR);
            if !clock::wait_until(|| HITS.load(Ordering::Relaxed) >= round, 1_000_000_000) {
                return Err("not delivered within 1 s".into());
            }
        }
        if super::count(arch::TEST_VECTOR) < 3 {
            return Err("handler count not updated".into());
        }
        Ok(())
    }
}
