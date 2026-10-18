//! In-kernel tests, run at boot when `plan9.ini` says `test=1`.
//!
//! These cover what host unit tests cannot: code that needs the real CPU,
//! its exception machinery and page tables. Each test prints one line;
//! `cargo xtask test` reads the console.

use crate::arch;
use crate::port::{clock, console, cpu, intr, mm, trap};
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
    ("trap: probe reads mapped memory", trap::ktests::probe_reads_mapped),
    (
        "trap: fixup recovers unmapped read",
        trap::ktests::probe_recovers_unmapped,
    ),
    (
        "trap: fixup recovers bad address",
        trap::ktests::probe_recovers_bad_address,
    ),
    ("mm: kernel stack has a guard page", mm::ktests::stack_has_guard),
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
