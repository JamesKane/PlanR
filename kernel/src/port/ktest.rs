//! In-kernel tests, run at boot when `plan9.ini` says `test=1`.
//!
//! These cover what host unit tests cannot: code that needs the real CPU,
//! its exception machinery and page tables. Each test prints one line;
//! `cargo xtask test` reads the console.

use crate::arch;
use crate::port::{chan, clock, console, cpu, exec, heap, intr, mm, mmu, proc, qlock, syscall, trap, uaccess};
use crate::println;

/// Why a test did not pass.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Fail {
    Failed(&'static str),
    /// The machine lacks what the test needs; not a failure.
    Skipped(&'static str),
}

impl From<&'static str> for Fail {
    fn from(why: &'static str) -> Fail {
        Fail::Failed(why)
    }
}

pub(crate) type TestResult = Result<(), Fail>;
pub(crate) type TestFn = fn() -> TestResult;

const TESTS: &[(&str, TestFn)] = &[
    ("trap: breakpoint resumes", trap::ktests::breakpoint_resumes),
    (
        "trap: backtrace symbols name functions",
        trap::ktests::symbols_name_functions,
    ),
    ("trap: probe reads mapped memory", trap::ktests::probe_reads_mapped),
    (
        "trap: fixup recovers unmapped read",
        trap::ktests::probe_recovers_unmapped,
    ),
    (
        "trap: fixup recovers bad address",
        trap::ktests::probe_recovers_bad_address,
    ),
    (
        "trap: user page guarded from the kernel",
        trap::ktests::user_page_guarded,
    ),
    ("mm: frames free and align", mm::ktests::frames_free_and_align),
    ("mm: kernel stack has a guard page", mm::ktests::stack_has_guard),
    ("mm: freed stacks are reused", mm::ktests::stacks_free),
    ("mmu: kernel text translates", mmu::ktests::image_translates),
    (
        "proc: own context cannot sleep",
        proc::ktests::own_context_not_sleepable,
    ),
    ("proc: threads take turns", proc::ktests::threads_take_turns),
    ("proc: higher priority runs first", proc::ktests::priority_first),
    ("proc: the clock preempts", proc::ktests::clock_preempts),
    ("proc: state survives a switch", proc::ktests::state_survives_switch),
    ("proc: dead threads are freed", proc::ktests::dead_threads_freed),
    ("proc: sleep and wakeup", proc::ktests::sleep_and_wakeup),
    ("proc: rendezvous swaps values", proc::ktests::rendezvous_swaps),
    ("qlock: excludes", qlock::ktests::qlock_excludes),
    ("qlock: rwlock excludes", qlock::ktests::rwlock_excludes),
    ("mmu: address spaces are separate", mmu::ktests::spaces_are_separate),
    ("uaccess: copies are checked", uaccess::ktests::copies_checked),
    (
        "syscall: user threads make system calls",
        syscall::ktests::user_threads_call,
    ),
    (
        "syscall: a user fault ends the thread",
        syscall::ktests::user_fault_kills,
    ),
    ("chan: names clean lexically", chan::ktests::names_clean),
    ("exec: programs run from the boot archive", exec::ktests::programs_run),
    ("exec: programs' memory is freed", exec::ktests::memory_freed),
    ("heap: collections allocate", heap::ktests::collections_work),
    ("heap: empty slabs return", heap::ktests::slabs_return),
    ("heap: large aligned block", heap::ktests::large_aligned),
    ("console: text drawn on the screen", console::ktests::screen_shows_text),
    (
        "console: output kept in the log ring",
        console::ktests::ring_records_output,
    ),
    ("clock: counter advances", clock::ktests::counter_advances),
    ("intr: self-IPI delivered", intr::ktests::self_ipi_delivered),
    ("clock: ticks arrive at HZ", clock::ktests::ticks_arrive),
    ("smp: every listed CPU online", cpu::ktests::all_online),
    ("smp: every CPU's clock ticks", cpu::ktests::each_cpu_ticks),
    ("smp: a call runs on its target", cpu::ktests::call_runs_on_target),
    ("smp: spin lock serializes CPUs", cpu::ktests::lock_serializes),
    (
        "smp: TLB shootdown reaches every CPU",
        mmu::ktests::shootdown_reaches_all,
    ),
];

/// Runs every test, portable then architecture-specific, and returns the
/// number that failed.
pub(crate) fn run() -> usize {
    let (mut passed, mut failed, mut skipped) = (0, 0, 0);
    for (name, test) in TESTS.iter().chain(arch::KTESTS) {
        match test() {
            Ok(()) => {
                println!("ktest: {name}: ok");
                passed += 1;
            }
            Err(Fail::Skipped(why)) => {
                println!("ktest: {name}: skipped: {why}");
                skipped += 1;
            }
            Err(Fail::Failed(why)) => {
                println!("ktest: {name}: FAILED: {why}");
                failed += 1;
            }
        }
    }
    println!("ktest: {passed} passed, {failed} failed, {skipped} skipped");
    failed
}
