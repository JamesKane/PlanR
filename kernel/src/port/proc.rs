//! Processes and the scheduler: kernel threads, each with its own stack
//! and saved context, run from per-CPU queues by priority, preempted by
//! the clock, and put to sleep and woken through [`Rendez`].
//!
//! **Own contexts.** A CPU starts out in its own context, the one it
//! booted or was started in. It is no [`Proc`] and runs below every
//! priority: only when nothing on the CPU is ready, as an idle loop would.
//! It cannot sleep.
//!
//! **Queues.** Each CPU has a queue per priority (Plan 9's twenty, highest
//! first; round robin within one). A thread that becomes ready goes to the
//! least loaded CPU, which gets a wake-up IPI if it is idle or running
//! something of lower priority. An idle CPU takes work from other CPUs'
//! queues. A *wired* thread stays on its CPU. There is no priority aging
//! yet, so a busy high-priority thread starves lower ones on its CPU.
//!
//! **Preemption.** Every tick, a thread that has used its quantum is
//! marked for rescheduling if something of at least its priority is
//! ready. The switch happens on the way out of the interrupt, after the
//! end-of-interrupt ([`preempt`]). Every spin lock holds interrupts off, so
//! an interrupt never arrives while one is held, and preempting there is
//! always safe. Interrupt handlers run on the interrupted thread's stack,
//! which keeps the interrupted state until the thread resumes.
//!
//! **User threads.** A user thread is a thread with a user address space,
//! its [`Memory`]. It starts by leaving the kernel for user mode, and comes
//! back on traps and system calls. Switching to a thread activates its
//! space (a kernel thread's is the kernel space), loads its user registers
//! and points the CPU's way in from user mode at its kernel stack. `exec`
//! gives it new memory ([`exec`]); its memory is freed once it is dead and
//! switched away from.
//!
//! **Children.** A user thread started by another thread is its child.
//! When it exits, its parent gets a [`Wait`] message, which [`await_child`]
//! returns, as Plan 9's `await` does.
//!
//! **Sleeps cut short.** A sleep may end early for a note
//! ([`Rendez::sleep_intr`]) or at a time ([`Rendez::tsleep`]). The note's
//! poster or the clock's process readies the sleeper only while it is
//! armed for that, under its `wake` lock, so a thread that has gone on to
//! sleep elsewhere (in a `QLock`, which takes no spurious wake-ups) is
//! never woken by mistake.
//!
//! **Switching.** A thread being left is still running on its own stack
//! until the switch has saved it. So it goes back on a queue (or, if dead,
//! is freed) only from the context switched to, in [`finish_switch`], and
//! its `on_cpu` flag stays set until then. A thread woken while still
//! switching out can be queued at once, but no CPU resumes it before
//! `on_cpu` clears. Only a CPU's own context waits for that: a CPU that
//! is leaving a thread holds that thread's flag, so two of them each
//! waiting for the other's thread would wait for ever. It puts the thread
//! back and goes on as if nothing were ready.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crate::arch::{self, context::Context, context::UserRegs};
use crate::port::clock;
use crate::port::cpu::{self, MAXCPU};
use crate::port::error::{EINTR, Error};
use crate::port::lock::SpinLock;
use crate::port::mm::Stack;
use crate::port::mmu;
use crate::port::note::{self, Notes};
use crate::port::pgrp::{Fgrp, Files};
use crate::port::segment::Memory;
use crate::port::timer;

/// Priorities, 0 (lowest) to `NPRI - 1`, as in Plan 9.
pub(crate) const NPRI: usize = 20;
/// Ordinary processes.
pub(crate) const PRI_NORMAL: usize = 10;
/// Kernel processes.
pub(crate) const PRI_KPROC: usize = 13;
/// Ticks a thread runs before it yields to its equals.
const QUANTUM: u32 = 2;
/// "Not wired to a CPU."
const UNWIRED: usize = usize::MAX;
/// The most wait messages kept for a parent that does not await them.
const MAX_WAITS: usize = 128;

/// What a process is doing.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// On a run queue.
    Ready,
    Running,
    /// Waiting to be woken ([`ready`]).
    Sleeping,
    /// Finished; freed once switched away from.
    Dead,
}

/// A process: a thread of control with its own kernel stack.
pub(crate) struct Proc {
    pid: u64,
    /// Its program's name, once it has one.
    name: SpinLock<String>,
    pri: AtomicUsize,
    /// The CPU it must run on, or [`UNWIRED`].
    wired: usize,
    state: AtomicU8,
    /// True from when a CPU starts running it until the context switched
    /// to has saved it.
    on_cpu: AtomicBool,
    /// Where it last ran.
    last_cpu: AtomicUsize,
    /// Saved while the process is not running.
    context: UnsafeCell<Context>,
    /// Freed with the process.
    stack: Stack,
    /// What the thread runs; taken when it starts.
    entry: SpinLock<Option<Entry>>,
    /// True for a user thread.
    is_user: bool,
    /// A user thread's memory, until it is dead and switched away from.
    /// Changed only by the thread itself (exec) and by the switch that
    /// leaves it dead.
    mem: SpinLock<Option<Arc<Memory>>>,
    /// A user thread's registers that the switch keeps. Touched only by
    /// the CPU running the thread, with interrupts off.
    user: UnsafeCell<UserRegs>,
    /// The error text of the last failed system call (`errstr`).
    errstr: SpinLock<Vec<u8>>,
    /// The exit status, once `exits` or a fatal trap has set it.
    status: SpinLock<Option<Vec<u8>>>,
    /// A user thread's name space and files, until it exits.
    files: SpinLock<Option<Files>>,
    /// Who hears when it exits.
    parent: Option<Weak<Proc>>,
    /// The parent's pid, or 0.
    ppid: u64,
    /// Its note group: its own pid, its parent's group, or a new one
    /// (`RFNOTEG`).
    noteid: AtomicU64,
    notes: SpinLock<Notes>,
    /// Set when a note is posted or the process is killed, cleared once
    /// [`note::notify`] finds nothing left to do: read on every way out to
    /// user mode.
    note_waiting: AtomicBool,
    /// Plan 9's `notepending`: a note came, and an interruptible sleep
    /// should end. Cleared by the sleep it ends.
    interrupted: AtomicBool,
    /// Whether a note or a timer may wake the thread now.
    wake: SpinLock<Wake>,
    /// The pending alarm: when it is due and its timer.
    alarm: SpinLock<Option<(u64, u64)>>,
    /// The arguments it was started with, separated by spaces.
    args: SpinLock<Vec<u8>>,
    children: SpinLock<Children>,
    /// Woken when a child exits.
    child_exited: Rendez,
    /// When it was made, for [`Wait::times`].
    started_ns: u64,
}

/// What may end the sleep a thread is in.
#[derive(Debug, Default)]
struct Wake {
    /// Numbers the thread's sleeps, so a timer set for one does not end
    /// another.
    seq: u64,
    /// The thread sleeps in sleep `seq` and may be readied for its note or
    /// timer.
    armed: bool,
    /// A note ends it.
    intr: bool,
}

/// A thread's children: how many are alive, and the wait messages of those
/// that have exited.
#[derive(Debug)]
struct Children {
    live: usize,
    exited: VecDeque<Wait>,
}

/// What a parent learns of a child that exited.
#[derive(Debug, Clone)]
pub(crate) struct Wait {
    pub(crate) pid: u64,
    /// User, system and real time in milliseconds. Only the real time is
    /// counted yet.
    pub(crate) times: [u64; 3],
    /// Empty for a normal exit, else `name pid: status`.
    pub(crate) msg: Vec<u8>,
}

/// What a new thread runs.
enum Entry {
    /// A kernel thread's function.
    Kernel(Box<dyn FnOnce() + Send>),
    /// A user thread's first instruction, stack pointer and argument.
    User { pc: u64, sp: u64, arg: u64 },
    /// A child of `rfork`, returning from the system call with the user
    /// frame at the top of its stack.
    Fork,
}

// SAFETY: `context` and `user` are touched only by the CPU running the
// process or switching away from or to it, and `on_cpu` keeps that to one
// CPU at a time (see the module comment).
unsafe impl Sync for Proc {}
// SAFETY: as above; the entry closure is `Send`.
unsafe impl Send for Proc {}

impl core::fmt::Debug for Proc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Proc")
            .field("pid", &self.pid)
            .field("name", &*self.name.lock())
            .field("pri", &self.pri())
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl Proc {
    pub(crate) fn pid(&self) -> u64 {
        self.pid
    }

    pub(crate) fn name(&self) -> String {
        self.name.lock().clone()
    }

    /// A user thread's memory.
    pub(crate) fn memory(&self) -> Option<Arc<Memory>> {
        self.mem.lock().clone()
    }

    pub(crate) fn state(&self) -> State {
        match self.state.load(Ordering::Acquire) {
            0 => State::Ready,
            1 => State::Running,
            2 => State::Sleeping,
            _ => State::Dead,
        }
    }

    fn set_state(&self, s: State) {
        self.state.store(s as u8, Ordering::Release);
    }

    /// The error text of the last failed system call.
    pub(crate) fn errstr(&self) -> &SpinLock<Vec<u8>> {
        &self.errstr
    }

    /// The parent's pid, or 0 if it had none (or was not to hear of its
    /// exit).
    pub(crate) fn ppid(&self) -> u64 {
        self.ppid
    }

    /// Its note group.
    pub(crate) fn noteid(&self) -> u64 {
        self.noteid.load(Ordering::Relaxed)
    }

    /// Starts a new note group (`RFNOTEG`).
    pub(crate) fn new_noteg(&self) {
        self.noteid
            .store(NEXT_PID.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Joins note group `id`.
    pub(crate) fn set_noteid(&self, id: u64) {
        self.noteid.store(id, Ordering::Relaxed);
    }

    pub(crate) fn is_user(&self) -> bool {
        self.is_user
    }

    pub(crate) fn pri(&self) -> usize {
        self.pri.load(Ordering::Relaxed)
    }

    /// Sets its priority, clamped to the range, as Plan 9's
    /// `procpriority`; it takes effect the next time it is queued.
    pub(crate) fn set_pri(&self, pri: usize) {
        self.pri.store(pri.min(NPRI - 1), Ordering::Relaxed);
    }

    /// When it was made, in counter nanoseconds.
    pub(crate) fn started_ns(&self) -> u64 {
        self.started_ns
    }

    pub(crate) fn notes(&self) -> &SpinLock<Notes> {
        &self.notes
    }

    /// The arguments it was started with, separated by spaces.
    pub(crate) fn args(&self) -> Vec<u8> {
        self.args.lock().clone()
    }

    /// Keeps `argv` for `#p/n/args`, as Plan 9's `procargs` gives them:
    /// each quoted as `%q` quotes, with blanks between.
    pub(crate) fn set_args(&self, argv: &[Vec<u8>]) {
        let mut text = Vec::new();
        for (i, a) in argv.iter().enumerate() {
            if i > 0 {
                text.push(b' ');
            }
            // `%q`: quoted if empty, or holding a quote or anything up to a
            // blank; a quote inside is doubled.
            if a.is_empty() || a.iter().any(|&c| c <= b' ' || c == b'\'') {
                text.push(b'\'');
                for &c in a {
                    if c == b'\'' {
                        text.push(c);
                    }
                    text.push(c);
                }
                text.push(b'\'');
            } else {
                text.extend_from_slice(a);
            }
        }
        *self.args.lock() = text;
    }

    /// Marks a note posted: an interruptible sleep ends, and the way out to
    /// user mode looks at the notes.
    pub(crate) fn interrupt(self: &Arc<Proc>) {
        self.note_waiting.store(true, Ordering::SeqCst);
        self.interrupted.store(true, Ordering::SeqCst);
        let w = self.wake.lock();
        if w.armed && w.intr {
            ready(self.clone());
        }
    }

    /// Forgets that a note came, once [`note::notify`] takes the notes in
    /// hand (Plan 9's `popnote`): a sleep after is not cut short for them.
    pub(crate) fn clear_interrupted(&self) {
        self.interrupted.store(false, Ordering::SeqCst);
    }

    /// Records whether notes still wait, once [`note::notify`] has looked.
    pub(crate) fn notes_checked(&self) {
        let n = self.notes.lock();
        self.note_waiting.store(n.waiting(), Ordering::SeqCst);
    }

    /// Readies the thread if it still sleeps in sleep `seq`: its timer
    /// came due.
    pub(crate) fn timer_wake(self: &Arc<Proc>, seq: u64) {
        let w = self.wake.lock();
        if w.armed && w.seq == seq {
            ready(self.clone());
        }
    }

    /// Sets the alarm to go off at `when` (0 for none) and returns when the
    /// previous one was due (0 if none).
    pub(crate) fn set_alarm(self: &Arc<Proc>, when: u64) -> u64 {
        let new = (when != 0).then(|| (when, timer::add(when, self, timer::What::Alarm)));
        let old = core::mem::replace(&mut *self.alarm.lock(), new);
        old.map_or(0, |(due, id)| {
            timer::cancel(id);
            due
        })
    }

    /// The alarm due at `when` went off: posts `alarm` if it is still set.
    pub(crate) fn alarm_fired(self: &Arc<Proc>, when: u64) {
        let fired = {
            let mut a = self.alarm.lock();
            let due = a.is_some_and(|(w, _)| w == when);
            if due {
                *a = None;
            }
            due
        };
        if fired {
            note::post(self, b"alarm", note::From::User);
        }
    }

    /// A user thread's name space and files.
    pub(crate) fn files(&self) -> &SpinLock<Option<Files>> {
        &self.files
    }

    /// The exit status, if it has exited through `exits` or a trap.
    pub(crate) fn status(&self) -> Option<Vec<u8>> {
        self.status.lock().clone()
    }
}

/// How to make a thread.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spawn<'a> {
    pub(crate) name: &'a str,
    pub(crate) pri: usize,
    /// The CPU to keep it on, if any.
    pub(crate) wired: Option<usize>,
}

impl<'a> Spawn<'a> {
    /// A kernel process: [`PRI_KPROC`], not wired.
    pub(crate) fn kproc(name: &'a str) -> Spawn<'a> {
        Spawn {
            name,
            pri: PRI_KPROC,
            wired: None,
        }
    }
}

static NEXT_PID: AtomicU64 = AtomicU64::new(1);

/// One CPU's ready threads, by priority.
#[derive(Debug)]
struct RunQueue {
    q: [VecDeque<Arc<Proc>>; NPRI],
}

impl RunQueue {
    const fn new() -> RunQueue {
        RunQueue {
            q: [const { VecDeque::new() }; NPRI],
        }
    }

    /// The highest priority with a ready thread.
    fn top(&self) -> Option<usize> {
        (0..NPRI).rev().find(|&p| !self.q[p].is_empty())
    }

    /// Takes the first thread of the highest priority at least `min`,
    /// skipping wired threads if `stealing`.
    fn take(&mut self, min: usize, stealing: bool) -> Option<Arc<Proc>> {
        (min..NPRI).rev().find_map(|p| {
            let q = &mut self.q[p];
            let i = q.iter().position(|t| !stealing || t.wired == UNWIRED)?;
            q.remove(i)
        })
    }
}

static QUEUES: [SpinLock<RunQueue>; MAXCPU] = [const { SpinLock::new(RunQueue::new()) }; MAXCPU];
/// Ready threads on each CPU's queue, read without its lock.
static QUEUED: [AtomicUsize; MAXCPU] = [const { AtomicUsize::new(0) }; MAXCPU];
/// The priority each CPU is running at, plus one; 0 for its own context.
static RUNNING_PRI: [AtomicUsize; MAXCPU] = [const { AtomicUsize::new(0) }; MAXCPU];
/// Set to make a CPU reschedule at its next interrupt exit.
static NEED_RESCHED: [AtomicBool; MAXCPU] = [const { AtomicBool::new(false) }; MAXCPU];

/// How the thread being left goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leave {
    /// Back on this CPU's queue.
    Requeue,
    /// Asleep: whoever wakes it queues it.
    Sleep,
    /// Dead: freed.
    Dead,
}

/// A CPU's switching state, touched only by that CPU with interrupts off.
#[derive(Debug)]
struct Local {
    /// The CPU's own context, saved while a thread runs.
    own: Context,
    /// The running thread, or `None` for the CPU's own context.
    current: Option<Arc<Proc>>,
    /// The thread just left, and how it goes on.
    left: Option<(Arc<Proc>, Leave)>,
    /// Ticks left in the current thread's quantum.
    quantum: u32,
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
            left: None,
            quantum: 0,
        })
    }; MAXCPU],
);

/// This CPU's [`Local`].
///
/// # Safety
///
/// Interrupts are off, and the reference is not kept across a switch (the
/// thread may resume on another CPU).
unsafe fn local() -> &'static mut Local {
    // SAFETY: the caller's contract: only this CPU uses its slot, and
    // nothing else on it can run until interrupts are back on.
    unsafe { &mut *LOCALS.0[cpu::current().index()].get() }
}

/// Makes a kernel thread that runs `f`, and makes it ready.
pub(crate) fn spawn(how: Spawn<'_>, f: impl FnOnce() + Send + 'static) -> Option<Arc<Proc>> {
    let b = Birth {
        mem: None,
        files: None,
        parent: None,
        noteid: None,
        notes: Notes::new(),
        args: Vec::new(),
        user: UserRegs::INITIAL,
        entry: Entry::Kernel(Box::new(f)),
        reserved: 0,
    };
    Some(make(how, Stack::new()?, b))
}

/// Makes a user thread in `mem`, with `files`, that starts at `pc` with
/// stack pointer `sp` and `arg` in the first argument register, and makes
/// it ready. It is a child of the calling thread, if any.
pub(crate) fn spawn_user(
    how: Spawn<'_>,
    mem: Arc<Memory>,
    files: Files,
    pc: u64,
    sp: u64,
    arg: u64,
) -> Option<Arc<Proc>> {
    let b = Birth {
        mem: Some(mem),
        files: Some(files),
        parent: current(),
        noteid: None,
        notes: Notes::new(),
        args: Vec::new(),
        user: UserRegs::INITIAL,
        entry: Entry::User { pc, sp, arg },
        reserved: 0,
    };
    Some(make(how, Stack::new()?, b))
}

/// Makes a child of the calling user thread, in a system call, as `rfork`
/// does, and makes it ready: in `mem`, with `files`, it returns from the
/// call with 0 and the caller's registers otherwise. Its parent hears of
/// its exit if `wait`; it starts a note group of its own if `noteg`.
pub(crate) fn fork(mem: Arc<Memory>, files: Files, wait: bool, noteg: bool) -> Option<Arc<Proc>> {
    let me = current().filter(|p| p.is_user)?;
    let stack = Stack::new()?;
    // SAFETY: a system call entered at the top of the caller's stack; the
    // child's is new and ours.
    let reserved = unsafe { arch::context::fork_frame(me.stack.top(), stack.top()) };
    let pl = arch::splhi();
    // SAFETY: only this CPU, running the thread, touches `user`, and
    // interrupts are off.
    let user = unsafe { (*me.user.get()).snapshot() };
    arch::splx(pl);
    let name = me.name();
    let inherited = me.notes.lock().inherit();
    let how = Spawn {
        name: &name,
        pri: me.pri(),
        wired: None,
    };
    let b = Birth {
        mem: Some(mem),
        files: Some(files),
        noteid: (!noteg).then(|| me.noteid()),
        notes: inherited,
        args: me.args(),
        parent: wait.then_some(me),
        user,
        entry: Entry::Fork,
        reserved,
    };
    Some(make(how, stack, b))
}

/// What a new thread is made with.
struct Birth {
    mem: Option<Arc<Memory>>,
    files: Option<Files>,
    /// Who hears when it exits.
    parent: Option<Arc<Proc>>,
    /// Its note group, if not a new one.
    noteid: Option<u64>,
    notes: Notes,
    args: Vec<u8>,
    user: UserRegs,
    entry: Entry,
    /// Bytes at the top of the stack already in use: a forked child's
    /// user frame.
    reserved: u64,
}

fn make(how: Spawn<'_>, stack: Stack, b: Birth) -> Arc<Proc> {
    assert!(how.pri < NPRI, "priority out of range");
    // SAFETY: the stack is new and ours, and nothing else uses what lies
    // below `reserved`.
    let context = unsafe { Context::new(stack.top() - b.reserved, thread_start, 0) };
    let wired = how.wired.unwrap_or(UNWIRED);
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let p = Arc::new(Proc {
        pid,
        name: SpinLock::new(String::from(how.name)),
        pri: AtomicUsize::new(how.pri),
        wired,
        state: AtomicU8::new(State::Sleeping as u8),
        on_cpu: AtomicBool::new(false),
        last_cpu: AtomicUsize::new(if wired == UNWIRED {
            cpu::current().index()
        } else {
            wired
        }),
        context: UnsafeCell::new(context),
        stack,
        entry: SpinLock::new(Some(b.entry)),
        is_user: b.mem.is_some(),
        mem: SpinLock::new(b.mem),
        user: UnsafeCell::new(b.user),
        errstr: SpinLock::new(Vec::new()),
        status: SpinLock::new(None),
        files: SpinLock::new(b.files),
        parent: b.parent.as_ref().map(Arc::downgrade),
        ppid: b.parent.as_ref().map_or(0, |p| p.pid),
        noteid: AtomicU64::new(b.noteid.unwrap_or(pid)),
        notes: SpinLock::new(b.notes),
        note_waiting: AtomicBool::new(false),
        interrupted: AtomicBool::new(false),
        wake: SpinLock::new(Wake::default()),
        alarm: SpinLock::new(None),
        args: SpinLock::new(b.args),
        children: SpinLock::new(Children {
            live: 0,
            exited: VecDeque::new(),
        }),
        child_exited: Rendez::new(),
        started_ns: clock::now_ns(),
    });
    if let Some(parent) = b.parent {
        parent.children.lock().live += 1;
    }
    PROCS.lock().insert(pid, Arc::downgrade(&p));
    ready(p.clone());
    p
}

/// Every thread not yet exited, by pid.
static PROCS: SpinLock<BTreeMap<u64, Weak<Proc>>> = SpinLock::new(BTreeMap::new());

/// The thread `pid`, if it has not exited.
pub(crate) fn find(pid: u64) -> Option<Arc<Proc>> {
    PROCS.lock().get(&pid).and_then(Weak::upgrade)
}

/// Every thread not yet exited, by pid.
pub(crate) fn all() -> Vec<Arc<Proc>> {
    PROCS.lock().values().filter_map(Weak::upgrade).collect()
}

/// True if the calling thread has notes for [`note::notify`] to look at:
/// asked on every way out to user mode, so it takes no lock.
pub(crate) fn notes_waiting() -> bool {
    let pl = arch::splhi();
    // SAFETY: interrupts are off, and the reference is dropped at once.
    let w = unsafe { local() }
        .current
        .as_ref()
        .is_some_and(|p| p.note_waiting.load(Ordering::SeqCst));
    arch::splx(pl);
    w
}

/// The thread running on this CPU, if any.
pub(crate) fn current() -> Option<Arc<Proc>> {
    let pl = arch::splhi();
    // SAFETY: interrupts are off, and the reference is dropped at once.
    let p = unsafe { local() }.current.clone();
    arch::splx(pl);
    p
}

/// Makes a sleeping (or new) thread ready: queues it on a CPU, and has
/// that CPU reschedule if the thread should run there now. Does nothing if
/// it is not asleep.
pub(crate) fn ready(p: Arc<Proc>) {
    if p.state
        .compare_exchange(
            State::Sleeping as u8,
            State::Ready as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    let target = choose_cpu(&p);
    let pri = p.pri();
    let pl = arch::splhi();
    enqueue(target, p);
    let me = cpu::current().index();
    if RUNNING_PRI[target].load(Ordering::Acquire) <= pri {
        NEED_RESCHED[target].store(true, Ordering::Release);
        if target != me
            && let Some(c) = cpu::get(target)
        {
            arch::send_ipi(c, arch::WAKE_VECTOR);
        }
    }
    arch::splx(pl);
}

/// Where a ready thread should go: its wired CPU, or the online CPU with
/// the least to do, preferring the one it last ran on.
fn choose_cpu(p: &Proc) -> usize {
    if p.wired != UNWIRED {
        return p.wired;
    }
    let last = p.last_cpu.load(Ordering::Relaxed);
    let load = |i: usize| QUEUED[i].load(Ordering::Relaxed) + usize::from(RUNNING_PRI[i].load(Ordering::Relaxed) != 0);
    cpu::online()
        .map(cpu::Cpu::index)
        .min_by_key(|&i| (load(i), i != last))
        .unwrap_or(last)
}

fn enqueue(cpu: usize, p: Arc<Proc>) {
    let pri = p.pri();
    QUEUES[cpu].lock().q[pri].push_back(p);
    QUEUED[cpu].fetch_add(1, Ordering::Relaxed);
}

/// The next thread for this CPU at priority `min` or above: from its own
/// queue, else taken from another CPU's.
fn pick(me: usize, min: usize) -> Option<Arc<Proc>> {
    let mine = QUEUES[me].lock().take(min, false);
    if let Some(p) = mine {
        QUEUED[me].fetch_sub(1, Ordering::Relaxed);
        return Some(p);
    }
    for c in cpu::online().map(cpu::Cpu::index).filter(|&c| c != me) {
        if QUEUED[c].load(Ordering::Relaxed) == 0 {
            continue;
        }
        let Some(mut q) = QUEUES[c].try_lock() else {
            continue;
        };
        if let Some(p) = q.take(min, true) {
            QUEUED[c].fetch_sub(1, Ordering::Relaxed);
            return Some(p);
        }
    }
    None
}

/// True if this CPU has a thread it could run now.
pub(crate) fn has_work() -> bool {
    let me = cpu::current().index();
    QUEUED[me].load(Ordering::Relaxed) != 0
        || cpu::online().any(|c| c.index() != me && QUEUED[c.index()].load(Ordering::Relaxed) != 0)
}

/// Passes the CPU to a ready thread of at least the caller's priority, if
/// there is one, and returns when this context runs again. From a CPU's
/// own context, that is once nothing is ready.
pub(crate) fn yield_now() {
    let pl = arch::splhi();
    let dead = reschedule(Leave::Requeue);
    arch::splx(pl);
    drop(dead);
}

/// Called on the way out of every interrupt, after its end-of-interrupt,
/// with interrupts off: switches if this CPU was asked to.
pub(crate) fn preempt() {
    let me = cpu::current().index();
    if NEED_RESCHED[me].swap(false, Ordering::AcqRel) {
        // The interrupted code held no spin lock (see the module comment).
        // A dead thread's free waits here with interrupts off, which
        // `mmu::shootdown` allows.
        drop(reschedule(Leave::Requeue));
    }
}

/// The clock tick, in its interrupt: counts down the quantum and asks
/// for a reschedule once it is used up and an equal is waiting.
pub(crate) fn tick() {
    let me = cpu::current().index();
    // SAFETY: an interrupt handler runs with interrupts off.
    let l = unsafe { local() };
    let min = match &l.current {
        Some(p) => {
            l.quantum = l.quantum.saturating_sub(1);
            if l.quantum > 0 {
                return;
            }
            p.pri()
        }
        None => 0,
    };
    let waiting = QUEUES[me].lock().top().is_some_and(|t| t >= min);
    if waiting || (l.current.is_none() && has_work()) {
        NEED_RESCHED[me].store(true, Ordering::Release);
    }
}

/// Ends the calling thread with exit status `status`.
pub(crate) fn exits(status: Vec<u8>) -> ! {
    if let Some(me) = current() {
        *me.status.lock() = Some(status);
    }
    exit()
}

/// Sets the calling user thread's thread pointer.
pub(crate) fn set_tls(tls: u64) {
    let pl = arch::splhi();
    // SAFETY: interrupts are off, and the reference is dropped at once.
    if let Some(me) = &unsafe { local() }.current {
        // SAFETY: only this CPU, running the thread, touches `user`, and
        // interrupts are off.
        unsafe { (*me.user.get()).set_tls(tls) };
    }
    arch::splx(pl);
}

/// Gives the calling thread new memory, in which it starts at `pc` with
/// stack pointer and first argument `sp`, with fresh user registers: the
/// end of `exec`. Nothing is left on the caller's kernel stack that needs
/// dropping, since the thread starts over at its top.
pub(crate) fn exec(mem: Arc<Memory>, name: String, pc: u64, sp: u64) -> ! {
    let me = current().expect("exec comes from a thread");
    assert!(me.is_user, "exec in a kernel thread");
    *me.name.lock() = name;
    let pl = arch::splhi();
    // SAFETY: only this CPU, running the thread, touches `user`, and
    // interrupts are off.
    unsafe { *me.user.get() = UserRegs::INITIAL };
    let old = me.mem.lock().replace(mem);
    // Off the old space before it can be freed.
    enter_space(Some(&*me));
    arch::splx(pl);
    drop(old);
    let top = me.stack.top();
    drop(me);
    // SAFETY: the new space is active, and holds `pc` and `sp`; nothing on
    // this stack is needed again.
    unsafe { arch::context::enter_user(pc, sp, sp, top) }
}

/// Waits for a child of the calling thread to exit and returns its wait
/// message, or `None` if it has no children left to wait for. A note cuts
/// the wait short.
pub(crate) fn await_child(s: &Sleepable) -> Result<Option<Wait>, Error> {
    let me = current().expect("a Sleepable proves a thread");
    await_of(&me, s)
}

/// Waits for a child of `p` to exit, as [`await_child`] does for the
/// caller (`#p/n/wait`).
pub(crate) fn await_of(p: &Proc, s: &Sleepable) -> Result<Option<Wait>, Error> {
    p.child_exited.sleep_intr(s, || {
        let c = p.children.lock();
        !c.exited.is_empty() || c.live == 0
    })?;
    Ok(p.children.lock().exited.pop_front())
}

/// Tells the calling thread's parent, if it has one still, that it is
/// exiting.
fn tell_parent(me: &Proc) {
    let Some(parent) = me.parent.as_ref().and_then(Weak::upgrade) else {
        return;
    };
    let status = me.status.lock().clone().unwrap_or_default();
    let mut msg = Vec::new();
    if !status.is_empty() {
        msg = alloc::format!("{} {}: ", me.name(), me.pid).into_bytes();
        msg.extend_from_slice(&status);
        msg.truncate(abi::ERRMAX - 1);
    }
    let real = (clock::now_ns() - me.started_ns) / 1_000_000;
    let w = Wait {
        pid: me.pid,
        times: [0, 0, real],
        msg,
    };
    {
        let mut c = parent.children.lock();
        c.live -= 1;
        if c.exited.len() < MAX_WAITS {
            c.exited.push_back(w);
        }
    }
    parent.child_exited.wakeup_all();
}

/// Ends the calling thread.
fn exit() -> ! {
    if let Some(me) = current() {
        // The descriptors go first, while the rest is still there for
        // their devices to use (an `RCLOSE` file in `#e`).
        let empty = Fgrp::new();
        let fds = me.files.lock().as_mut().map(|f| core::mem::replace(&mut f.fgrp, empty));
        drop(fds);
        let files = me.files.lock().take();
        drop(files);
        if let Some((_, id)) = me.alarm.lock().take() {
            timer::cancel(id);
        }
        PROCS.lock().remove(&me.pid);
        tell_parent(&me);
    }
    let _ = arch::splhi();
    drop(reschedule(Leave::Dead));
    unreachable!("a dead thread was resumed");
}

/// Switches away from the current context, if `how` and the queues say
/// so, and returns once it runs again, with the thread left behind by the
/// switch into it, if dead, for the caller to drop with interrupts on.
/// Interrupts must be off.
fn reschedule(how: Leave) -> Reaped {
    let me = cpu::current().index();
    // SAFETY: interrupts are off; `l` is not used after the switch.
    let l = unsafe { local() };
    let min = match (&l.current, how) {
        (Some(p), Leave::Requeue) => p.pri(),
        (None, Leave::Requeue) | (Some(_), _) => 0,
        (None, _) => panic!("a CPU's own context cannot sleep or exit"),
    };
    let mut next = pick(me, min);
    if let (Some(n), Some(c)) = (&next, &l.current)
        && Arc::ptr_eq(n, c)
    {
        // Woken while going to sleep, and queued here: run on.
        c.set_state(State::Running);
        return None;
    }
    if l.current.is_some()
        && let Some(n) = next.take_if(|n| n.on_cpu.load(Ordering::Acquire))
    {
        // Still being saved elsewhere: this CPU's own context waits for
        // it (see the module comment), or a later switch takes it.
        enqueue(me, n);
        NEED_RESCHED[me].store(true, Ordering::Release);
    }
    let (prev, to) = match next {
        Some(next) => {
            // Wait for the CPU that last ran it to finish saving it, which
            // only this CPU's own context comes to.
            while next.on_cpu.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            next.on_cpu.store(true, Ordering::Relaxed);
            next.set_state(State::Running);
            next.last_cpu.store(me, Ordering::Relaxed);
            RUNNING_PRI[me].store(next.pri() + 1, Ordering::Release);
            let to = next.context.get().cast_const();
            l.quantum = QUANTUM;
            (l.current.replace(next), to)
        }
        None if how == Leave::Requeue => return None,
        None => {
            RUNNING_PRI[me].store(0, Ordering::Release);
            (l.current.take(), (&raw const l.own))
        }
    };
    let from = match prev {
        Some(p) => {
            let from = p.context.get();
            if how == Leave::Requeue {
                p.set_state(State::Ready);
            } else if how == Leave::Dead {
                p.set_state(State::Dead);
            }
            l.left = Some((p, how));
            from
        }
        None => &raw mut l.own,
    };
    if let Some((p, _)) = &l.left
        && p.is_user
    {
        // SAFETY: this CPU ran it until now; interrupts are off.
        unsafe { (*p.user.get()).save() };
    }
    enter_space(l.current.as_deref());
    // SAFETY: `to` is a context saved by a switch or made new, and only this
    // CPU resumes it (`on_cpu`); `from` is held in `left` or is this CPU's
    // own, so it stays valid while it is saved.
    unsafe { arch::context::switch(from, to) };
    finish_switch()
}

/// Makes the calling CPU ready to run `p` (or its own context): its
/// address space, its user registers, and its kernel stack as the way in
/// from user mode. Interrupts are off.
fn enter_space(p: Option<&Proc>) {
    let Some(p) = p else {
        mmu::activate_kernel_space();
        return;
    };
    match &*p.mem.lock() {
        Some(mem) => {
            // SAFETY: `p` holds its memory until exec has activated new
            // memory, or a switch away from it, dead, has activated
            // another space.
            unsafe { mem.space().activate() };
            // SAFETY: `p` is about to run here; no other CPU touches it.
            unsafe { (*p.user.get()).restore() };
        }
        None => mmu::activate_kernel_space(),
    }
    arch::context::set_kernel_stack(p.stack.top());
}

/// A dead thread and the memory it ran in, freed when dropped.
type Reaped = Option<(Arc<Proc>, Option<Arc<Memory>>)>;

/// Run in the context switched to, interrupts off: finishes with the
/// thread left behind. Returns it and its memory if it is dead, to be
/// dropped (freeing its stack and memory) once interrupts are back on,
/// whoever else still holds the thread.
fn finish_switch() -> Reaped {
    let me = cpu::current().index();
    // SAFETY: interrupts are off, and the reference is dropped here.
    let left = unsafe { local() }.left.take();
    let (p, how) = left?;
    p.on_cpu.store(false, Ordering::Release);
    match how {
        Leave::Requeue => {
            enqueue(me, p);
            None
        }
        Leave::Sleep => None,
        Leave::Dead => {
            let mem = p.mem.lock().take();
            Some((p, mem))
        }
    }
}

/// Where a new thread starts, with interrupts off from the switch.
extern "C" fn thread_start(_: usize) -> ! {
    let dead = finish_switch();
    arch::spllo();
    drop(dead);
    let me = current().expect("a thread runs as itself");
    let entry = me.entry.lock().take().expect("a thread starts once");
    let top = me.stack.top();
    drop(me);
    match entry {
        Entry::Kernel(f) => {
            f();
            exit()
        }
        // SAFETY: nothing on this stack is needed again; the switch to
        // this thread activated `space`, where `pc` and `sp` are.
        Entry::User { pc, sp, arg } => unsafe { arch::context::enter_user(pc, sp, arg, top) },
        // SAFETY: `fork` wrote the frame at the top of the stack, above
        // all this thread has used; the switch activated its space.
        Entry::Fork => unsafe { arch::context::resume_user(top) },
    }
}

/// Proof that the caller is a thread that may sleep: not a CPU's own
/// context, not an interrupt handler, no spin lock held (interrupts on).
/// Locks that sleep take one ([`crate::port::qlock`]).
#[derive(Debug)]
pub(crate) struct Sleepable {
    /// Neither `Send` nor `Sync`: it belongs to the thread that made it.
    _thread: PhantomData<*mut ()>,
}

/// A [`Sleepable`], if the caller may sleep.
pub(crate) fn sleepable() -> Option<Sleepable> {
    (arch::interrupts_enabled() && current().is_some()).then_some(Sleepable { _thread: PhantomData })
}

/// Where threads sleep until a condition holds (Plan 9's `Rendez`, which
/// here may have several sleepers).
#[derive(Debug)]
pub(crate) struct Rendez {
    sleepers: SpinLock<VecDeque<Arc<Proc>>>,
}

impl Rendez {
    pub(crate) const fn new() -> Rendez {
        Rendez {
            sleepers: SpinLock::new(VecDeque::new()),
        }
    }

    /// Sleeps until `cond` holds. `cond` is checked under the rendez's
    /// lock, so a waker that makes it true and then calls
    /// [`wakeup`](Rendez::wakeup) is never missed.
    pub(crate) fn sleep(&self, s: &Sleepable, cond: impl Fn() -> bool) {
        self.sleep_for(s, cond, false, None);
    }

    /// Sleeps until `cond` holds, or fails with "interrupted" if a note
    /// comes first.
    pub(crate) fn sleep_intr(&self, s: &Sleepable, cond: impl Fn() -> bool) -> Result<(), Error> {
        match self.sleep_for(s, cond, true, None) {
            Woke::Cond => Ok(()),
            _ => Err(EINTR.into()),
        }
    }

    /// Sleeps until `cond` holds or the counter reaches `deadline` (in
    /// nanoseconds, [`clock::now_ns`]); a note ends it too if `intr`.
    pub(crate) fn tsleep(&self, s: &Sleepable, cond: impl Fn() -> bool, deadline: u64, intr: bool) -> Woke {
        self.sleep_for(s, cond, intr, Some(deadline))
    }

    fn sleep_for(&self, _: &Sleepable, cond: impl Fn() -> bool, intr: bool, deadline: Option<u64>) -> Woke {
        let me = current().expect("a Sleepable proves a thread");
        let seq = {
            let mut w = me.wake.lock();
            w.seq += 1;
            w.seq
        };
        let timer = deadline.map(|d| timer::add(d, &me, timer::What::Wake(seq)));
        let ended = || {
            if intr && me.interrupted.load(Ordering::SeqCst) {
                Some(Woke::Interrupted)
            } else if deadline.is_some_and(|d| clock::now_ns() >= d) {
                Some(Woke::Timeout)
            } else {
                None
            }
        };
        // Whether it was asleep in the queue, and so may still be there.
        let mut queued = false;
        let woke = loop {
            let pl = arch::splhi();
            let mut q = self.sleepers.lock();
            // Taken off the queue by a waker, rather than readied by a note
            // or a timer.
            let mut popped = false;
            if queued {
                let n = q.len();
                q.retain(|p| !Arc::ptr_eq(p, &me));
                popped = q.len() == n;
                queued = false;
            }
            let end = if cond() { Some(Woke::Cond) } else { ended() };
            if let Some(end) = end {
                drop(q);
                arch::splx(pl);
                if popped && end != Woke::Cond {
                    // The wake-up was meant for a sleeper: pass it on.
                    self.wakeup();
                }
                break end;
            }
            me.set_state(State::Sleeping);
            q.push_back(me.clone());
            {
                let mut w = me.wake.lock();
                // A note posted, or the timer run, before the arming: no
                // one will ready it, and only it can still change its state.
                if ended().is_some() {
                    drop(w);
                    q.pop_back();
                    me.set_state(State::Running);
                    drop(q);
                    arch::splx(pl);
                    continue;
                }
                w.armed = true;
                w.intr = intr;
            }
            drop(q);
            let dead = reschedule(Leave::Sleep);
            me.wake.lock().armed = false;
            arch::splx(pl);
            drop(dead);
            queued = true;
        };
        if let Some(id) = timer {
            timer::cancel(id);
        }
        if woke == Woke::Interrupted {
            me.interrupted.store(false, Ordering::SeqCst);
        }
        woke
    }

    /// Wakes the first sleeper, if any; true if there was one.
    pub(crate) fn wakeup(&self) -> bool {
        let p = self.sleepers.lock().pop_front();
        p.map(ready).is_some()
    }

    /// Wakes every sleeper and returns how many there were.
    pub(crate) fn wakeup_all(&self) -> usize {
        let all = core::mem::take(&mut *self.sleepers.lock());
        let n = all.len();
        all.into_iter().for_each(ready);
        n
    }
}

/// How a sleep that may be cut short ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Woke {
    /// Its condition held.
    Cond,
    /// A note came.
    Interrupted,
    /// Its time ran out.
    Timeout,
}

/// Puts the calling thread to sleep, having queued it somewhere a waker
/// will find it, inside `queue` (run under the caller's lock, with
/// interrupts off). For [`crate::port::qlock`].
pub(crate) fn sleep_queued(_: &Sleepable, queue: impl FnOnce(Arc<Proc>)) {
    let pl = arch::splhi();
    // SAFETY: interrupts are off; dropped before the switch.
    let me = unsafe { local() }.current.clone().expect("a Sleepable proves a thread");
    me.set_state(State::Sleeping);
    queue(me);
    let dead = reschedule(Leave::Sleep);
    arch::splx(pl);
    drop(dead);
}

/// A meeting place for [`Rgrp::rendezvous`].
#[derive(Debug)]
struct Meet {
    tag: u64,
    value: u64,
    /// The value handed back, once `done`.
    reply: AtomicU64,
    done: AtomicBool,
    r: Rendez,
}

/// A rendezvous group: the processes whose tags meet in
/// [`rendezvous`](Rgrp::rendezvous). Children share their parent's unless
/// `rfork(RFREND)` starts a new one.
#[derive(Debug)]
pub(crate) struct Rgrp {
    meets: SpinLock<Vec<Arc<Meet>>>,
}

impl Rgrp {
    pub(crate) const fn new() -> Rgrp {
        Rgrp {
            meets: SpinLock::new(Vec::new()),
        }
    }

    /// Plan 9's `rendezvous`: the first caller with `tag` waits, and the
    /// second wakes it; each returns the other's `value`. A note ends the
    /// wait, unless the other has come already.
    pub(crate) fn rendezvous(&self, s: &Sleepable, tag: u64, value: u64) -> Result<u64, Error> {
        let mut meets = self.meets.lock();
        if let Some(i) = meets.iter().position(|m| m.tag == tag) {
            let m = meets.swap_remove(i);
            drop(meets);
            m.reply.store(value, Ordering::Relaxed);
            m.done.store(true, Ordering::Release);
            m.r.wakeup();
            return Ok(m.value);
        }
        let m = Arc::new(Meet {
            tag,
            value,
            reply: AtomicU64::new(0),
            done: AtomicBool::new(false),
            r: Rendez::new(),
        });
        meets.push(m.clone());
        drop(meets);
        if m.r.sleep_intr(s, || m.done.load(Ordering::Acquire)).is_err() {
            let mut meets = self.meets.lock();
            if let Some(i) = meets.iter().position(|x| Arc::ptr_eq(x, &m)) {
                meets.swap_remove(i);
                return Err(EINTR.into());
            }
            drop(meets);
            // The other took it, and is about to say so.
            m.r.sleep(s, || m.done.load(Ordering::Acquire));
        }
        Ok(m.reply.load(Ordering::Relaxed))
    }
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use super::{PRI_NORMAL, Rendez, Rgrp, Spawn, Woke, current, sleepable, spawn, yield_now};
    use crate::arch;
    use crate::port::clock;
    use crate::port::cpu;
    use crate::port::ktest::TestResult;
    use crate::port::lock::SpinLock;
    use crate::port::mm;

    const SECOND: u64 = 1_000_000_000;

    /// A normal-priority thread wired to the calling CPU.
    fn here(name: &str) -> Spawn<'_> {
        Spawn {
            name,
            pri: PRI_NORMAL,
            wired: Some(cpu::current().index()),
        }
    }

    /// Runs `f` (spawning a test's threads) with interrupts off, so that
    /// the calling context is not preempted until all are queued.
    fn together<R>(f: impl FnOnce() -> R) -> R {
        let pl = arch::splhi();
        let r = f();
        arch::splx(pl);
        r
    }

    /// Waits, idling, until `n` has reached `want`.
    fn wait_for(n: &AtomicUsize, want: usize) -> TestResult {
        if clock::wait_until(|| n.load(Ordering::Acquire) >= want, 5 * SECOND) {
            Ok(())
        } else {
            Err("threads did not finish within 5 s".into())
        }
    }

    /// Three threads that yield between steps take turns: each step of
    /// every thread happens before the step after next of any.
    pub(crate) fn threads_take_turns() -> TestResult {
        let log = Arc::new(SpinLock::new(Vec::new()));
        let done = Arc::new(AtomicUsize::new(0));
        together(|| -> TestResult {
            for name in ["a", "b", "c"] {
                let (log, done) = (log.clone(), done.clone());
                spawn(here(name), move || {
                    for step in 0..4 {
                        let me = current().expect("running as a thread");
                        assert_eq!(me.name(), name, "current() is the running thread");
                        log.lock().push((step, me.pid()));
                        drop(me);
                        yield_now();
                    }
                    done.fetch_add(1, Ordering::Release);
                })
                .ok_or("cannot spawn")?;
            }
            Ok(())
        })?;
        wait_for(&done, 3)?;
        let log = log.lock().clone();
        for (i, &(step, _)) in log.iter().enumerate() {
            if log[i..].iter().any(|&(s, _)| s + 2 <= step) {
                return Err("threads did not take turns".into());
            }
        }
        let mut pids: Vec<u64> = log.iter().map(|&(_, p)| p).collect();
        pids.sort_unstable();
        pids.dedup();
        if pids.len() != 3 {
            return Err("threads share a pid".into());
        }
        Ok(())
    }

    /// Callee-saved registers, integer and floating point, survive
    /// switches, voluntary and preempted.
    #[allow(clippy::cast_precision_loss)] // small seeds
    pub(crate) fn state_survives_switch() -> TestResult {
        fn work(seed: u64, yielding: bool) -> (u64, f64) {
            let (mut a, mut x) = (seed, seed as f64);
            for _ in 0..200_000 {
                a = a.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                x = x * 1.000_001 + 0.5;
                if yielding && a.is_multiple_of(4096) {
                    yield_now();
                }
            }
            (a, x)
        }
        let results = Arc::new(SpinLock::new(Vec::new()));
        let done = Arc::new(AtomicUsize::new(0));
        let n = cpu::online().count() * 2 + 2;
        for seed in 1..=n as u64 {
            let (results, done) = (results.clone(), done.clone());
            spawn(Spawn::kproc("work"), move || {
                let r = work(seed, seed % 2 == 0);
                results.lock().push((seed, r));
                done.fetch_add(1, Ordering::Release);
            })
            .ok_or("cannot spawn")?;
        }
        wait_for(&done, n)?;
        for &(seed, r) in results.lock().iter() {
            if r != work(seed, false) {
                return Err("a thread's registers changed across a switch".into());
            }
        }
        Ok(())
    }

    /// Dead threads' stacks go back: running many threads leaves the free
    /// frame count where it was.
    pub(crate) fn dead_threads_freed() -> TestResult {
        let run = || -> TestResult {
            let done = Arc::new(AtomicUsize::new(0));
            together(|| -> TestResult {
                for _ in 0..20 {
                    let done = done.clone();
                    spawn(here("short"), move || {
                        done.fetch_add(1, Ordering::Release);
                    })
                    .ok_or("cannot spawn")?;
                }
                Ok(())
            })?;
            wait_for(&done, 20)?;
            // The last thread's stack is freed by whoever runs after it.
            while super::has_work() {
                yield_now();
            }
            yield_now();
            Ok(())
        };
        // Once first, so the heap classes, the queues and the stack slot list
        // have grown to what the run needs.
        run()?;
        let before = mm::frame_counts().0;
        run()?;
        if mm::frame_counts().0 != before {
            return Err("dead threads' memory not freed".into());
        }
        Ok(())
    }

    /// Threads that never yield still share one CPU: the clock preempts
    /// them.
    pub(crate) fn clock_preempts() -> TestResult {
        let counts: Arc<[AtomicU64; 3]> = Arc::new([const { AtomicU64::new(0) }; 3]);
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicUsize::new(0));
        let pl = arch::splhi();
        for i in 0..3 {
            let (counts, stop, done) = (counts.clone(), stop.clone(), done.clone());
            spawn(here("spin"), move || {
                let end = clock::now_ns() + SECOND / 2;
                while !stop.load(Ordering::Acquire) {
                    counts[i].fetch_add(1, Ordering::Relaxed);
                    // The last to see every count move ends the test.
                    if counts.iter().all(|c| c.load(Ordering::Relaxed) > 0) || clock::now_ns() > end {
                        stop.store(true, Ordering::Release);
                    }
                }
                done.fetch_add(1, Ordering::Release);
            })
            .ok_or("cannot spawn")?;
        }
        arch::splx(pl);
        wait_for(&done, 3)?;
        if counts.iter().any(|c| c.load(Ordering::Relaxed) == 0) {
            return Err("a spinning thread never ran".into());
        }
        Ok(())
    }

    /// The higher priority runs first.
    pub(crate) fn priority_first() -> TestResult {
        let order = Arc::new(SpinLock::new(Vec::new()));
        let done = Arc::new(AtomicUsize::new(0));
        together(|| -> TestResult {
            for pri in [PRI_NORMAL, PRI_NORMAL + 2, PRI_NORMAL + 1] {
                let (order, done) = (order.clone(), done.clone());
                let how = Spawn { pri, ..here("pri") };
                spawn(how, move || {
                    order.lock().push(pri);
                    done.fetch_add(1, Ordering::Release);
                })
                .ok_or("cannot spawn")?;
            }
            Ok(())
        })?;
        wait_for(&done, 3)?;
        if *order.lock() != [PRI_NORMAL + 2, PRI_NORMAL + 1, PRI_NORMAL] {
            return Err("not run in priority order".into());
        }
        Ok(())
    }

    /// A sleeper wakes when another thread makes its condition true, on
    /// whichever CPUs they land.
    pub(crate) fn sleep_and_wakeup() -> TestResult {
        struct Shared {
            r: Rendez,
            turn: AtomicUsize,
        }
        const ROUNDS: usize = 200;
        let s = Arc::new(Shared {
            r: Rendez::new(),
            turn: AtomicUsize::new(0),
        });
        let done = Arc::new(AtomicUsize::new(0));
        // Two threads pass the turn back and forth.
        for me in 0..2 {
            let (s, done) = (s.clone(), done.clone());
            spawn(Spawn::kproc("pingpong"), move || {
                let sl = sleepable().expect("a thread may sleep");
                for _ in 0..ROUNDS {
                    s.r.sleep(&sl, || s.turn.load(Ordering::Acquire) % 2 == me);
                    s.turn.fetch_add(1, Ordering::AcqRel);
                    s.r.wakeup_all();
                }
                done.fetch_add(1, Ordering::Release);
            })
            .ok_or("cannot spawn")?;
        }
        wait_for(&done, 2)?;
        if s.turn.load(Ordering::Acquire) != 2 * ROUNDS {
            return Err("turns lost".into());
        }
        Ok(())
    }

    /// Two threads meet at a tag and swap values.
    pub(crate) fn rendezvous_swaps() -> TestResult {
        static RG: Rgrp = Rgrp::new();
        let got = Arc::new([const { AtomicU64::new(0) }; 2]);
        let done = Arc::new(AtomicUsize::new(0));
        for i in 0..2 {
            let (got, done) = (got.clone(), done.clone());
            spawn(Spawn::kproc("meet"), move || {
                let sl = sleepable().expect("a thread may sleep");
                let v = RG.rendezvous(&sl, 0x7a9, 100 + i as u64).unwrap_or(0);
                got[i].store(v, Ordering::Relaxed);
                done.fetch_add(1, Ordering::Release);
            })
            .ok_or("cannot spawn")?;
        }
        wait_for(&done, 2)?;
        if got[0].load(Ordering::Relaxed) != 101 || got[1].load(Ordering::Relaxed) != 100 {
            return Err("values not swapped".into());
        }
        Ok(())
    }

    /// A timed sleep whose condition never holds ends at its time, not
    /// before, and one whose condition comes true ends then.
    pub(crate) fn timed_sleep_ends() -> TestResult {
        let out = Arc::new(SpinLock::new(None));
        let done = Arc::new(AtomicUsize::new(0));
        let (o, d) = (out.clone(), done.clone());
        spawn(Spawn::kproc("tsleep"), move || {
            let sl = sleepable().expect("a thread may sleep");
            let r = Rendez::new();
            let start = clock::now_ns();
            let woke = r.tsleep(&sl, || false, start + 20_000_000, false);
            let took = clock::now_ns() - start;
            let early = r.tsleep(&sl, || true, clock::now_ns() + SECOND, false);
            *o.lock() = Some((woke, took, early));
            d.fetch_add(1, Ordering::Release);
        })
        .ok_or("cannot spawn")?;
        wait_for(&done, 1)?;
        let (woke, took, early) = out.lock().take().ok_or("no result")?;
        if woke != Woke::Timeout || took < 20_000_000 {
            return Err("a timed sleep did not last its time".into());
        }
        if early != Woke::Cond {
            return Err("a timed sleep outlasted its condition".into());
        }
        Ok(())
    }

    /// A CPU's own context cannot get a [`super::Sleepable`].
    pub(crate) fn own_context_not_sleepable() -> TestResult {
        if sleepable().is_some() {
            return Err("own context got a Sleepable".into());
        }
        Ok(())
    }
}
