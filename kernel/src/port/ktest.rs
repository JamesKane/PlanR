//! In-kernel tests, run at boot when `plan9.ini` says `test=1`.
//!
//! These cover what host unit tests cannot: code that needs the real CPU,
//! its exception machinery and page tables. Each test prints one line;
//! `cargo xtask test` reads the console.

use crate::port::trap;
use crate::println;

type TestFn = fn() -> Result<(), &'static str>;

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
];

/// Runs every test and returns the number that failed.
pub(crate) fn run() -> usize {
    let mut failed = 0;
    for (name, test) in TESTS {
        match test() {
            Ok(()) => println!("ktest: {name}: ok"),
            Err(why) => {
                println!("ktest: {name}: FAILED: {why}");
                failed += 1;
            }
        }
    }
    println!("ktest: {} passed, {failed} failed", TESTS.len() - failed);
    failed
}
