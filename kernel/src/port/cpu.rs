//! Processors: per-CPU state, starting the other CPUs, and asking another
//! CPU to run a function.
//!
//! CPU 0 is the boot CPU; the others are numbered as they are started, in
//! the order the MADT lists them (Plan 9's `machno`). Each CPU finds its own
//! [`Cpu`] through an architecture register: the `GS` base on amd64,
//! `TPIDR_EL1` on arm64.
//!
//! Until the scheduler exists an application processor sits in
//! [`idle_loop`], taking clock ticks and running whatever [`call`] hands it.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use crate::arch;
use crate::port::acpi::System;
use crate::port::clock;
use crate::println;

/// The most CPUs the kernel will run.
pub(crate) const MAXCPU: usize = 64;

/// How long a starting CPU has to report in.
const START_TIMEOUT_NS: u64 = 1_000_000_000;

/// One CPU's state.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct Cpu {
    /// Must stay first: amd64 reads it at `gs:0`.
    index: usize,
    /// The local APIC ID (amd64) or MPIDR affinity (arm64).
    hwid: AtomicU64,
    online: AtomicBool,
    pub(crate) clock: clock::Local,
    /// A function another CPU asked this one to run, or null.
    call: AtomicPtr<()>,
    /// Calls this CPU has finished.
    calls: AtomicU64,
}

impl Cpu {
    const fn new(index: usize) -> Cpu {
        Cpu {
            index,
            hwid: AtomicU64::new(0),
            online: AtomicBool::new(false),
            clock: clock::Local::new(),
            call: AtomicPtr::new(core::ptr::null_mut()),
            calls: AtomicU64::new(0),
        }
    }

    pub(crate) fn index(&self) -> usize {
        self.index
    }

    pub(crate) fn hwid(&self) -> u64 {
        self.hwid.load(Ordering::Relaxed)
    }

    pub(crate) fn is_online(&self) -> bool {
        self.online.load(Ordering::Acquire)
    }
}

static CPUS: [Cpu; MAXCPU] = {
    let mut cpus = [const { Cpu::new(0) }; MAXCPU];
    let mut i = 0;
    while i < MAXCPU {
        cpus[i].index = i;
        i += 1;
    }
    cpus
};

/// Slots handed out so far, including the boot CPU's.
static CLAIMED: AtomicUsize = AtomicUsize::new(1);
/// CPUs the firmware lists as usable, including the boot CPU.
static FOUND: AtomicUsize = AtomicUsize::new(1);

/// Makes CPU 0 the current CPU. Called first thing on the boot CPU, before
/// anything uses [`current`].
pub(crate) fn init_boot() {
    arch::set_cpu_local(&CPUS[0]);
}

/// The CPU this code is running on.
pub(crate) fn current() -> &'static Cpu {
    &CPUS[arch::cpu_index()]
}

pub(crate) fn get(index: usize) -> Option<&'static Cpu> {
    CPUS.get(index)
}

/// The CPUs that have come up, the boot CPU first.
pub(crate) fn online() -> impl Iterator<Item = &'static Cpu> {
    CPUS[..CLAIMED.load(Ordering::Acquire).min(MAXCPU)]
        .iter()
        .filter(|c| c.is_online())
}

/// How many CPUs the firmware listed as usable.
pub(crate) fn found() -> usize {
    FOUND.load(Ordering::Relaxed)
}

/// What the architecture's start-up code reports.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Census {
    /// Usable CPUs in the MADT, the boot CPU included.
    pub(crate) found: usize,
}

/// Starts every other CPU the firmware lists and reports how many came up.
pub(crate) fn start_others(acpi: Option<&System>) {
    let me = current();
    me.hwid.store(arch::hwid(), Ordering::Relaxed);
    me.online.store(true, Ordering::Release);
    if let Err(e) = arch::register_ipis() {
        println!("smp: {e}; staying on one CPU");
        return;
    }
    match arch::start_cpus(acpi) {
        Ok(Census { found }) => {
            FOUND.store(found, Ordering::Relaxed);
            println!("smp: {} of {found} CPUs online", online().count());
        }
        Err(e) => println!("smp: {e}; staying on one CPU"),
    }
}

/// Hands out the next CPU slot for a CPU with hardware ID `hwid`, or `None`
/// when all [`MAXCPU`] are taken.
pub(crate) fn claim(hwid: u64) -> Option<&'static Cpu> {
    let i = CLAIMED.fetch_add(1, Ordering::AcqRel);
    let Some(cpu) = CPUS.get(i) else {
        CLAIMED.store(MAXCPU, Ordering::Release);
        return None;
    };
    cpu.hwid.store(hwid, Ordering::Relaxed);
    Some(cpu)
}

/// Spins until `cpu` reports in or the start-up timeout passes.
pub(crate) fn wait_online(cpu: &Cpu) -> bool {
    clock::spin_until(|| cpu.is_online(), START_TIMEOUT_NS)
}

/// The portable half of an application processor's start-up, on its own
/// stack with the architecture's per-CPU state (traps, interrupt
/// controller, timer) ready.
pub(crate) fn ap_main(cpu: &'static Cpu) -> ! {
    clock::start();
    cpu.online.store(true, Ordering::Release);
    arch::spllo();
    idle_loop(cpu)
}

/// Runs calls as they arrive and otherwise waits for interrupts.
fn idle_loop(cpu: &Cpu) -> ! {
    loop {
        // With interrupts off between the check and the wait, a wake-up
        // IPI sent in between stays pending and ends the wait at once.
        let _ = arch::splhi();
        let f = cpu.call.swap(core::ptr::null_mut(), Ordering::Acquire);
        if f.is_null() {
            arch::idle();
            continue;
        }
        arch::spllo();
        // SAFETY: only `call` stores here, and it stores a `fn()`.
        let f = unsafe { core::mem::transmute::<*mut (), fn()>(f) };
        f();
        cpu.calls.fetch_add(1, Ordering::Release);
    }
}

/// Why [`call`] did not hand over the function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallError {
    NotOnline,
    /// The CPU has not yet taken the previous call.
    Busy,
    /// The CPU is the caller.
    Myself,
}

/// Asks CPU `index` to run `f` from its idle loop, and returns its count
/// of finished calls beforehand, for [`wait_calls`].
pub(crate) fn call(index: usize, f: fn()) -> Result<u64, CallError> {
    let cpu = get(index).filter(|c| c.is_online()).ok_or(CallError::NotOnline)?;
    if cpu.index == current().index {
        return Err(CallError::Myself);
    }
    let before = cpu.calls.load(Ordering::Acquire);
    cpu.call
        .compare_exchange(
            core::ptr::null_mut(),
            f as *mut (),
            Ordering::Release,
            Ordering::Relaxed,
        )
        .map_err(|_| CallError::Busy)?;
    arch::send_ipi(cpu, arch::WAKE_VECTOR);
    Ok(before)
}

/// Waits until CPU `index` has finished more than `before` calls.
pub(crate) fn wait_calls(index: usize, before: u64, timeout_ns: u64) -> bool {
    get(index).is_some_and(|c| clock::spin_until(|| c.calls.load(Ordering::Acquire) > before, timeout_ns))
}

/// The wake-up IPI's handler: it only ends the target's wait.
pub(crate) fn wake() {}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

    use super::{CallError, call, current, found, online, wait_calls};
    use crate::port::clock;
    use crate::port::ktest::{Fail, TestResult};
    use crate::port::lock::SpinLock;

    const SECOND: u64 = 1_000_000_000;

    fn need_others() -> TestResult {
        if online().count() > 1 {
            Ok(())
        } else {
            Err(Fail::Skipped("only one CPU"))
        }
    }

    /// Runs `check` on every online CPU, one at a time, and returns
    /// whether it held on all of them.
    pub(crate) fn holds_everywhere(check: fn() -> bool) -> Result<bool, &'static str> {
        static CHECK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
        static FAILED: AtomicBool = AtomicBool::new(false);
        fn run() {
            // SAFETY: only `holds_everywhere` stores here, and it stores a
            // `fn() -> bool`.
            let check = unsafe { core::mem::transmute::<*mut (), fn() -> bool>(CHECK.load(Ordering::Acquire)) };
            if !check() {
                FAILED.store(true, Ordering::Release);
            }
        }
        CHECK.store(check as *mut (), Ordering::Release);
        FAILED.store(false, Ordering::Relaxed);
        let here = check();
        for c in online().filter(|c| c.index() != current().index()) {
            let before = call(c.index(), run).map_err(|_| "call refused")?;
            if !wait_calls(c.index(), before, SECOND) {
                return Err("call not finished within 1 s");
            }
        }
        Ok(here && !FAILED.load(Ordering::Acquire))
    }

    pub(crate) fn all_online() -> TestResult {
        if found() <= 1 {
            return Err(Fail::Skipped("only one CPU"));
        }
        if online().count() == found().min(super::MAXCPU) {
            Ok(())
        } else {
            Err("some CPUs did not come up".into())
        }
    }

    /// Every CPU's own timer is running.
    pub(crate) fn each_cpu_ticks() -> TestResult {
        need_others()?;
        let start: [u64; super::MAXCPU] = core::array::from_fn(|i| super::get(i).map_or(0, |c| c.clock.ticks()));
        let advanced = || online().all(|c| c.clock.ticks() > start[c.index] + 1);
        if clock::wait_until(advanced, SECOND) {
            Ok(())
        } else {
            Err("a CPU's clock is not ticking".into())
        }
    }

    /// A call runs on the CPU it was sent to.
    pub(crate) fn call_runs_on_target() -> TestResult {
        static SEEN: AtomicU64 = AtomicU64::new(0);
        fn note() {
            SEEN.fetch_or(1 << current().index(), Ordering::Relaxed);
        }
        need_others()?;
        SEEN.store(0, Ordering::Relaxed);
        let mut want = 0u64;
        for c in online().filter(|c| c.index() != current().index()) {
            let before = call(c.index(), note).map_err(|_| "call refused")?;
            if !wait_calls(c.index(), before, SECOND) {
                return Err("call not finished within 1 s".into());
            }
            want |= 1 << c.index();
        }
        if call(current().index(), note) != Err(CallError::Myself) {
            return Err("call to self accepted".into());
        }
        if SEEN.load(Ordering::Relaxed) == want {
            Ok(())
        } else {
            Err("a call ran on the wrong CPU".into())
        }
    }

    /// Every CPU hammers one lock at once; no increment may be lost.
    pub(crate) fn lock_serializes() -> TestResult {
        const ROUNDS: u64 = 20_000;
        static COUNTER: SpinLock<u64> = SpinLock::new(0);
        fn hammer() {
            for _ in 0..ROUNDS {
                let mut n = COUNTER.lock();
                // A separate read and write, so a lock that let two CPUs in
                // at once would lose updates.
                let v = core::hint::black_box(*n);
                *n = v + 1;
            }
        }
        need_others()?;
        *COUNTER.lock() = 0;
        let mut started = [None; super::MAXCPU];
        for c in online().filter(|c| c.index() != current().index()) {
            started[c.index()] = Some(call(c.index(), hammer).map_err(|_| "call refused")?);
        }
        hammer();
        let mut n = 1;
        for (i, before) in started.iter().enumerate() {
            if let Some(before) = *before {
                if !wait_calls(i, before, 10 * SECOND) {
                    return Err("a CPU did not finish within 10 s".into());
                }
                n += 1;
            }
        }
        if *COUNTER.lock() == n * ROUNDS {
            Ok(())
        } else {
            Err("lost updates".into())
        }
    }
}
