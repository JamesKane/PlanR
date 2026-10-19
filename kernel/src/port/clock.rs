//! Time: a free-running counter and a periodic tick.
//!
//! The counter is the architecture's (the TSC on amd64, `CNTVCT_EL0` on
//! arm64). The tick is built on a one-shot timer re-armed from each
//! interrupt at an absolute deadline, so latency does not accumulate; the
//! same timer will drive a tickless scheduler later. Every CPU has its own
//! timer and tick; CPU 0's ticks are the system's, as in Plan 9.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch;
use crate::port::{cpu, proc, timer};

/// Ticks per second, as in Plan 9.
pub(crate) const HZ: u64 = 100;

const NS_PER_SEC: u64 = 1_000_000_000;

/// Counter frequency in Hz; 0 until [`init`].
static COUNTER_HZ: AtomicU64 = AtomicU64::new(0);
/// Counter units per tick.
static PERIOD: AtomicU64 = AtomicU64::new(0);
/// CPU 0's ticks.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// One CPU's tick.
#[derive(Debug)]
pub(crate) struct Local {
    ticks: AtomicU64,
    /// Counter value at which the next tick is due.
    next: AtomicU64,
}

impl Local {
    pub(crate) const fn new() -> Local {
        Local {
            ticks: AtomicU64::new(0),
            next: AtomicU64::new(0),
        }
    }

    /// Ticks this CPU has taken.
    pub(crate) fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }
}

/// Records the counter frequency the architecture measured or was told.
pub(crate) fn init(counter_hz: u64) {
    COUNTER_HZ.store(counter_hz, Ordering::Relaxed);
    PERIOD.store(counter_hz / HZ, Ordering::Relaxed);
}

pub(crate) fn counter_hz() -> u64 {
    COUNTER_HZ.load(Ordering::Relaxed)
}

/// Starts this CPU's tick. The architecture has routed its timer
/// interrupt to [`tick`].
pub(crate) fn start() {
    let next = arch::counter() + PERIOD.load(Ordering::Relaxed);
    cpu::current().clock.next.store(next, Ordering::Relaxed);
    arch::timer_at(next);
}

/// The timer interrupt.
pub(crate) fn tick() {
    let me = cpu::current();
    if me.index() == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    let local = &me.clock;
    local.ticks.fetch_add(1, Ordering::Relaxed);
    let period = PERIOD.load(Ordering::Relaxed);
    let mut next = local.next.load(Ordering::Relaxed) + period;
    let now = arch::counter();
    if next <= now {
        // Fell behind (a long stall, or a slow emulator): skip, don't burst.
        next = now + period;
    }
    local.next.store(next, Ordering::Relaxed);
    arch::timer_at(next);
    if me.index() == 0 {
        timer::tick();
    }
    proc::tick();
}

pub(crate) fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Converts counter units to nanoseconds.
pub(crate) fn to_ns(count: u64) -> u64 {
    let hz = counter_hz().max(1);
    u64::try_from(u128::from(count) * u128::from(NS_PER_SEC) / u128::from(hz)).unwrap_or(u64::MAX)
}

/// Nanoseconds since the counter started (usually power-on).
pub(crate) fn now_ns() -> u64 {
    to_ns(arch::counter())
}

/// Idles until `done` is true or `timeout_ns` passes; true if `done`.
/// Interrupts are on while waiting. With no interrupt source at all it
/// still returns, because the tick wakes the CPU.
pub(crate) fn wait_until(done: impl Fn() -> bool, timeout_ns: u64) -> bool {
    let end = now_ns().saturating_add(timeout_ns);
    while !done() {
        if now_ns() >= end {
            return done();
        }
        arch::idle();
    }
    true
}

/// Spins until `done` is true or `timeout_ns` passes; true if `done`.
/// For waits that must not depend on interrupts, or are shorter than a
/// tick.
pub(crate) fn spin_until(done: impl Fn() -> bool, timeout_ns: u64) -> bool {
    let end = now_ns().saturating_add(timeout_ns);
    while !done() {
        if now_ns() >= end {
            return done();
        }
        core::hint::spin_loop();
    }
    true
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use super::{HZ, NS_PER_SEC, now_ns, ticks, wait_until};
    use crate::arch;
    use crate::port::ktest::TestResult;

    pub(crate) fn counter_advances() -> TestResult {
        if super::counter_hz() == 0 {
            return Err("no counter frequency".into());
        }
        let a = arch::counter();
        for _ in 0..1_000_000 {
            if arch::counter() > a {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err("counter stuck".into())
    }

    /// Ten ticks arrive, at roughly `HZ`. The bounds are loose: emulators
    /// on a loaded host are slow, but a factor of four means a wrong
    /// frequency, not a busy machine.
    pub(crate) fn ticks_arrive() -> TestResult {
        const N: u64 = 10;
        let first = ticks();
        if !wait_until(|| ticks() > first, NS_PER_SEC) {
            return Err("no tick within 1 s".into());
        }
        let want = N * NS_PER_SEC / HZ;
        let (t0, start) = (ticks(), now_ns());
        if !wait_until(|| ticks() >= t0 + N, 8 * want) {
            return Err("ticks stopped".into());
        }
        let took = now_ns() - start;
        if took < want / 2 {
            return Err("ticks too fast".into());
        }
        if took > want * 4 {
            return Err("ticks too slow".into());
        }
        Ok(())
    }
}
