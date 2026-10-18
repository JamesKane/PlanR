//! Portable trap support: the fault-fixup table, fatal-trap reports and
//! symbolized frame-pointer backtraces. The architecture code decodes the
//! trap and calls in here.

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::{self, ExitCode};
use crate::port::console;
use crate::{PANIC_MARKER, println};

/// An address that is never mapped: the direct map ends long before it.
/// Used by the trap tests.
pub(crate) const UNMAPPED: u64 = 0xffff_9000_dead_0000;

/// Breakpoint traps taken; they are counted and resumed.
pub(crate) static BREAKPOINTS: AtomicUsize = AtomicUsize::new(0);

/// A fixup entry: if the instruction at `fault` faults, resume at `recover`
/// instead of treating the fault as fatal. Entries are emitted into the
/// `.fixup` section next to the instructions they cover.
#[repr(C)]
struct Fixup {
    fault: u64,
    recover: u64,
}

unsafe extern "C" {
    static __fixup_start: Fixup;
    static __fixup_end: Fixup;
    static __ksyms_start: u8;
    static __ksyms_end: u8;
}

/// The symbol table `xtask` wrote into the image after the link, or `None`
/// for an image built without it (the block is then zeros).
pub(crate) fn symbols() -> Option<ksyms::Table<'static>> {
    let start = &raw const __ksyms_start;
    let end = &raw const __ksyms_end;
    // SAFETY: the linker script reserves the bytes between the two symbols
    // in rodata. They are extern, so the compiler cannot assume their
    // contents are the zeros the linker put there.
    let block = unsafe { core::slice::from_raw_parts(start, end as usize - start as usize) };
    ksyms::Table::parse(block)
}

/// Where to resume if the instruction at `pc` faulted, if it has a fixup.
pub(crate) fn fixup(pc: u64) -> Option<u64> {
    let start = &raw const __fixup_start;
    let end = &raw const __fixup_end;
    let n = (end as usize - start as usize) / size_of::<Fixup>();
    // SAFETY: the linker script places `n` entries between the two symbols.
    let table = unsafe { core::slice::from_raw_parts(start, n) };
    table.iter().find(|f| f.fault == pc).map(|f| f.recover)
}

/// Enters panic mode, then reports an unrecoverable trap and powers off.
/// `regs` prints the saved register state.
pub(crate) fn fatal(what: &dyn fmt::Display, regs: &dyn fmt::Display, pc: u64, fp: u64) -> ! {
    if !console::begin_panic() {
        arch::exit(ExitCode::Failure);
    }
    println!("{PANIC_MARKER}: trap: {what}");
    println!("{regs}");
    backtrace(Some(pc), fp);
    arch::exit(ExitCode::Failure)
}

const MAX_FRAMES: usize = 32;

/// Prints the call chain by following frame records. Both architectures lay
/// a record out as `[fp] = caller's fp, [fp + 8] = return address`; the
/// kernel is built with frame pointers forced on. Every read goes through
/// [`arch::probe_read`], so a corrupt chain ends the walk instead of
/// faulting again.
pub(crate) fn backtrace(pc: Option<u64>, mut fp: u64) {
    println!("backtrace:");
    let symbols = symbols();
    let frame = |i: usize, addr: u64, within: u64| match symbols.and_then(|t| t.lookup(within)) {
        Some(s) => println!("  #{i} {addr:#018x} {}+{:#x}", s.name, addr - s.addr),
        None => println!("  #{i} {addr:#018x}"),
    };
    if let Some(pc) = pc {
        frame(0, pc, pc);
    }
    for i in 1..=MAX_FRAMES {
        if fp == 0 || !fp.is_multiple_of(8) {
            break;
        }
        let (Some(next), Some(ret)) = (arch::probe_read(fp), arch::probe_read(fp + 8)) else {
            println!("  (frame pointer {fp:#x} unreadable)");
            break;
        };
        if ret == 0 {
            break;
        }
        // A call can be a function's last instruction, so its return
        // address may lie past the end; the call itself is just before.
        frame(i, ret, ret - 1);
        // The stack grows down, so each caller's record is higher.
        if next <= fp {
            break;
        }
        fp = next;
    }
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use super::{BREAKPOINTS, UNMAPPED};
    use crate::arch;
    use crate::port::ktest::{Fail, TestResult};
    use crate::port::mm;
    use core::sync::atomic::Ordering;

    pub(crate) fn breakpoint_resumes() -> TestResult {
        let before = BREAKPOINTS.load(Ordering::Relaxed);
        arch::breakpoint();
        if BREAKPOINTS.load(Ordering::Relaxed) == before + 1 {
            Ok(())
        } else {
            Err("breakpoint not counted".into())
        }
    }

    /// The symbol table names this function: xtask wrote it, and the
    /// lookup finds functions.
    pub(crate) fn symbols_name_functions() -> TestResult {
        let table = super::symbols().ok_or("no symbol table: image not built by xtask")?;
        let f = symbols_name_functions as fn() -> TestResult as usize as u64;
        match table.lookup(f + 1) {
            Some(s) if s.addr == f && s.name == "kernel::port::trap::ktests::symbols_name_functions" => Ok(()),
            Some(_) => Err("wrong symbol".into()),
            None => Err("no symbol".into()),
        }
    }

    pub(crate) fn probe_reads_mapped() -> TestResult {
        static WORD: u64 = 0x9b00_7ed0_c0ff_ee00;
        match arch::probe_read((&raw const WORD) as u64) {
            Some(v) if v == WORD => Ok(()),
            Some(_) => Err("wrong value".into()),
            None => Err("mapped read faulted".into()),
        }
    }

    /// The user-access path reads a user page. With SMAP or PAN on, a
    /// plain kernel read of it faults.
    pub(crate) fn user_page_guarded() -> TestResult {
        let va = mm::ktests::user_page()?;
        if arch::probe_user_read(va) != Some(mm::ktests::USER_WORD) {
            return Err("user-access read failed".into());
        }
        if !arch::protect::user_access_guarded() {
            return Err(Fail::Skipped("no SMAP or PAN; the user-access read works"));
        }
        arch::probe_read(va).map_or(Ok(()), |_| Err("plain read of a user page succeeded".into()))
    }

    pub(crate) fn probe_recovers_unmapped() -> TestResult {
        arch::probe_read(UNMAPPED).map_or(Ok(()), |_| Err("unmapped read succeeded".into()))
    }

    /// Outside both translation ranges: non-canonical on amd64 (#GP), and
    /// neither all-zero nor all-one top bits on arm64.
    pub(crate) fn probe_recovers_bad_address() -> TestResult {
        arch::probe_read(0x8000_0000_0000_0000).map_or(Ok(()), |_| Err("bad-address read succeeded".into()))
    }
}

/// Counts a breakpoint trap. The architecture code resumes after it.
pub(crate) fn count_breakpoint() {
    BREAKPOINTS.fetch_add(1, Ordering::Relaxed);
}
