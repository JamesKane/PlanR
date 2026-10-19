//! Processes: for now, kernel threads, each with its own stack and saved
//! context, and cooperative switching between them.
//!
//! A CPU starts out in its own context, the one it booted or was started
//! in; it is no [`Proc`], and it runs only when no thread is ready, as an
//! idle loop would. [`yield_now`] passes the CPU to the next ready thread
//! and puts the current one at the back of the queue. A thread whose
//! function returns is dead, and its stack is freed.
//!
//! Switching is careful about one thing. The thread being left is still
//! running, on its own stack, until the switch has saved it. So it goes
//! back on the ready queue (or, if dead, is freed) only from the context
//! switched to, in [`finish_switch`]. Until then, no other CPU can take it.
//!
//! The scheduler proper (preemption, per-CPU queues, priorities, sleep and
//! wakeup) comes next and builds on this.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::{self, context::Context};
use crate::port::cpu::{self, MAXCPU};
use crate::port::lock::SpinLock;
use crate::port::mm::Stack;

/// A process: a thread of control with its own kernel stack.
pub(crate) struct Proc {
    pid: u64,
    name: String,
    /// Saved while the process is not running.
    context: UnsafeCell<Context>,
    stack: Stack,
    /// What the thread runs; taken when it starts.
    entry: SpinLock<Option<Entry>>,
}

type Entry = Box<dyn FnOnce() + Send>;

// SAFETY: `context` is touched only by the CPU switching away from or to
// the process, and the switch protocol (see the module comment) lets one
// CPU at a time do so.
unsafe impl Sync for Proc {}
// SAFETY: as above; the entry closure is `Send`.
unsafe impl Send for Proc {}

impl core::fmt::Debug for Proc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Proc")
            .field("pid", &self.pid)
            .field("name", &self.name)
            .field("stack", &self.stack)
            .finish_non_exhaustive()
    }
}

impl Proc {
    pub(crate) fn pid(&self) -> u64 {
        self.pid
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

static NEXT_PID: AtomicU64 = AtomicU64::new(1);
/// Threads ready to run.
static READY: SpinLock<VecDeque<Arc<Proc>>> = SpinLock::new(VecDeque::new());

/// A CPU's switching state, touched only by that CPU with interrupts off.
#[derive(Debug)]
struct Local {
    /// The CPU's own context, saved while a thread runs.
    own: Context,
    /// The running thread, or `None` for the CPU's own context.
    current: Option<Arc<Proc>>,
    /// The thread just left, to go back on the ready queue.
    requeue: Option<Arc<Proc>>,
    /// The thread just left for good, to be freed.
    dead: Option<Arc<Proc>>,
}

/// Per-CPU [`Local`]s.
struct Locals([UnsafeCell<Local>; MAXCPU]);

// SAFETY: each CPU touches only its own slot, with interrupts off.
unsafe impl Sync for Locals {}

static LOCALS: Locals = Locals(
    [const {
        UnsafeCell::new(Local {
            own: Context::EMPTY,
            current: None,
            requeue: None,
            dead: None,
        })
    }; MAXCPU],
);

/// This CPU's [`Local`].
///
/// # Safety
///
/// Interrupts are off, and the reference is not kept across a switch.
unsafe fn local() -> &'static mut Local {
    // SAFETY: the caller's contract: only this CPU uses its slot, and
    // nothing else on it can run until interrupts are back on.
    unsafe { &mut *LOCALS.0[cpu::current().index()].get() }
}

/// Makes a kernel thread that runs `f`, and puts it on the ready queue.
pub(crate) fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Option<Arc<Proc>> {
    let stack = Stack::new()?;
    // SAFETY: the stack is new and ours.
    let context = unsafe { Context::new(stack.top(), thread_start, 0) };
    let p = Arc::new(Proc {
        pid: NEXT_PID.fetch_add(1, Ordering::Relaxed),
        name: String::from(name),
        context: UnsafeCell::new(context),
        stack,
        entry: SpinLock::new(Some(Box::new(f))),
    });
    READY.lock().push_back(p.clone());
    Some(p)
}

/// The thread running on this CPU, if any.
pub(crate) fn current() -> Option<Arc<Proc>> {
    let pl = arch::splhi();
    // SAFETY: interrupts are off, and the reference is dropped at once.
    let p = unsafe { local() }.current.clone();
    arch::splx(pl);
    p
}

/// Passes the CPU to the next ready thread, if there is one, and returns
/// when this context is next chosen. From a CPU's own context, that is
/// once no thread is ready.
pub(crate) fn yield_now() {
    let pl = arch::splhi();
    let next = READY.lock().pop_front();
    if let Some(next) = next {
        // SAFETY: interrupts are off.
        let l = unsafe { local() };
        let prev = l.current.take();
        l.requeue.clone_from(&prev);
        // SAFETY: `prev` is held in `requeue` until `finish_switch`, so its
        // context stays valid while it is saved.
        unsafe { switch_to(prev.as_deref(), next) };
    }
    arch::splx(pl);
    finish_switch();
}

/// Ends the calling thread: switches to the next ready one, or to the
/// CPU's own context, and has its stack freed there.
fn exit() -> ! {
    let _ = arch::splhi();
    let next = READY.lock().pop_front();
    // SAFETY: interrupts are off.
    let l = unsafe { local() };
    let me = l.current.take().expect("a CPU's own context cannot exit");
    let from = me.context.get();
    // Held until the next context frees it, so `from` stays valid while the
    // switch saves into it.
    l.dead = Some(me);
    let to = match next {
        Some(next) => {
            let to = next.context.get().cast_const();
            l.current = Some(next);
            to
        }
        // The CPU's own context, saved when it last switched to a thread.
        None => &raw const l.own,
    };
    // SAFETY: `to` is a saved or new context only this CPU resumes.
    unsafe { arch::context::switch(from, to) };
    unreachable!("a dead thread was resumed");
}

/// Switches from `prev` (or the CPU's own context) to `next`.
///
/// # Safety
///
/// Interrupts are off, and `prev` lives until the next context runs
/// [`finish_switch`].
unsafe fn switch_to(prev: Option<&Proc>, next: Arc<Proc>) {
    // SAFETY: interrupts are off.
    let l = unsafe { local() };
    let from = match prev {
        Some(p) => p.context.get(),
        None => &raw mut l.own,
    };
    let to = next.context.get().cast_const();
    l.current = Some(next);
    // SAFETY: `to` is a saved or new context that only this CPU resumes,
    // since `next` came off the ready queue; `from` lives on.
    unsafe { arch::context::switch(from, to) };
}

/// Run in the context switched to: puts the thread left behind back on
/// the ready queue, or frees it if it is dead. Interrupts must be on, or
/// the stack's free (a TLB shootdown) could wait on a CPU waiting on this
/// one.
fn finish_switch() {
    let pl = arch::splhi();
    // SAFETY: interrupts are off, and the reference is dropped here.
    let (requeue, dead) = unsafe {
        let l = local();
        (l.requeue.take(), l.dead.take())
    };
    if let Some(p) = requeue {
        READY.lock().push_back(p);
    }
    arch::splx(pl);
    drop(dead);
}

/// Where a new thread starts, with interrupts off from the switch.
extern "C" fn thread_start(_: usize) -> ! {
    arch::spllo();
    finish_switch();
    let me = current().expect("a thread runs as itself");
    let f = me.entry.lock().take().expect("a thread starts once");
    drop(me);
    f();
    exit()
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use super::{current, spawn, yield_now};
    use crate::port::ktest::TestResult;
    use crate::port::lock::SpinLock;
    use crate::port::mm;

    /// Three threads that yield between steps take turns, and the CPU's
    /// own context resumes once all have finished.
    pub(crate) fn threads_take_turns() -> TestResult {
        let log = Arc::new(SpinLock::new(Vec::new()));
        for name in ["a", "b", "c"] {
            let log = log.clone();
            spawn(name, move || {
                for step in 0..3 {
                    let me = current().expect("running as a thread");
                    log.lock().push((me.name().as_bytes()[0], step, me.pid()));
                    drop(me);
                    yield_now();
                }
            })
            .ok_or("cannot spawn")?;
        }
        yield_now();
        if current().is_some() {
            return Err("the CPU's own context resumed as a thread".into());
        }
        let got: Vec<(u8, i32)> = log.lock().iter().map(|&(n, s, _)| (n, s)).collect();
        let want: Vec<(u8, i32)> = (0..3).flat_map(|s| [(b'a', s), (b'b', s), (b'c', s)]).collect();
        if got != want {
            return Err("threads did not take turns".into());
        }
        let pids: Vec<u64> = log.lock().iter().take(3).map(|&(_, _, p)| p).collect();
        if pids[0] == pids[1] || pids[1] == pids[2] || pids[0] == pids[2] {
            return Err("threads share a pid".into());
        }
        Ok(())
    }

    /// Callee-saved registers, integer and floating point, survive a
    /// switch: a thread computes across yields while others do the same.
    #[allow(clippy::cast_precision_loss)] // small seeds
    pub(crate) fn state_survives_switch() -> TestResult {
        fn work(seed: u64) -> (u64, f64) {
            let (mut a, mut x) = (seed, seed as f64);
            for _ in 0..50 {
                a = a.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                x = x * 1.000_001 + 0.5;
                yield_now();
            }
            (a, x)
        }
        fn expected(seed: u64) -> (u64, f64) {
            let (mut a, mut x) = (seed, seed as f64);
            for _ in 0..50 {
                a = a.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                x = x * 1.000_001 + 0.5;
            }
            (a, x)
        }
        let results = Arc::new(SpinLock::new(Vec::new()));
        for seed in 1..=4u64 {
            let results = results.clone();
            spawn("work", move || {
                let r = work(seed);
                results.lock().push((seed, r));
            })
            .ok_or("cannot spawn")?;
        }
        yield_now();
        let got = results.lock().clone();
        if got.len() != 4 {
            return Err("not every thread finished".into());
        }
        for (seed, r) in got {
            if r != expected(seed) {
                return Err("a thread's registers changed across a switch".into());
            }
        }
        Ok(())
    }

    /// Dead threads' stacks go back: running many threads leaves the free
    /// frame count where it was.
    pub(crate) fn dead_threads_freed() -> TestResult {
        // Once first, so the heap classes, the ready queue and the stack slot
        // list have grown to what the run needs.
        for _ in 0..20 {
            spawn("warm", || {}).ok_or("cannot spawn")?;
        }
        yield_now();
        let before = mm::frame_counts().0;
        for _ in 0..20 {
            spawn("short", || {}).ok_or("cannot spawn")?;
        }
        yield_now();
        if mm::frame_counts().0 != before {
            return Err("dead threads' memory not freed".into());
        }
        Ok(())
    }
}
