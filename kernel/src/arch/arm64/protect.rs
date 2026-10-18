//! CPU protections: PAN (the kernel cannot read or write user pages
//! outside a window that clears it), turned on where `ID_AA64MMFR1_EL1`
//! reports it. PXN and UXN are set on every kernel mapping by 9boot and
//! [`super::mmu::map_page`].
//!
//! BTI is not turned on: it needs `bti` landing pads at every indirect
//! branch target, and the stable toolchain can neither emit them
//! (`-Z branch-protection` is nightly only) nor rebuild `core` with them.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::port::ktest::{Fail, TestResult};
use crate::println;

/// `SCTLR_EL1.SPAN`: when set, taking an exception to EL1 leaves PAN alone.
/// Cleared, so every exception entry sets PAN.
const SCTLR_SPAN: u64 = 1 << 23;

/// True once PAN is on. The PAN instructions are undefined on a CPU
/// without it, so the user-access path reads this byte first.
pub(super) static PAN: AtomicBool = AtomicBool::new(false);

fn supported() -> bool {
    let v: u64;
    // SAFETY: reads an ID register.
    unsafe { asm!("mrs {}, id_aa64mmfr1_el1", out(reg) v, options(nomem, nostack)) };
    (v >> 20) & 0xf != 0
}

/// Turns on the calling CPU's protections. Every CPU calls it; the
/// application processors also inherit the boot CPU's `SCTLR_EL1`.
pub(crate) fn init_cpu() {
    if !supported() {
        return;
    }
    // SAFETY: PAN faults only on kernel use of user pages, and nothing in
    // the kernel touches one outside the user-access path (9boot maps
    // none). The isb completes the SCTLR write before PAN is set.
    unsafe {
        asm!(
            ".arch_extension pan",
            "mrs {t}, sctlr_el1",
            "and {t}, {t}, {keep}",
            "msr sctlr_el1, {t}",
            "isb",
            "msr pan, #1",
            t = out(reg) _,
            keep = in(reg) !SCTLR_SPAN,
            options(nostack),
        );
    }
    PAN.store(true, Ordering::Relaxed);
}

/// True if a plain kernel access to a user page faults.
pub(crate) fn user_access_guarded() -> bool {
    PAN.load(Ordering::Relaxed)
}

/// Prints which protections are on.
pub(crate) fn report() {
    let pan = if user_access_guarded() { "on" } else { "unsupported" };
    println!("cpu: PAN {pan}, BTI off (no toolchain support)");
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use super::{Fail, SCTLR_SPAN, TestResult, asm, supported};
    use crate::port::cpu::ktests::holds_everywhere;

    /// PAN is set, and exception entry keeps it set.
    fn pan_set() -> bool {
        let (pan, sctlr): (u64, u64);
        // SAFETY: reads system registers.
        unsafe {
            asm!(
                ".arch_extension pan",
                "mrs {}, pan",
                "mrs {}, sctlr_el1",
                out(reg) pan,
                out(reg) sctlr,
                options(nomem, nostack),
            );
        }
        pan & (1 << 22) != 0 && sctlr & SCTLR_SPAN == 0
    }

    pub(crate) fn pan() -> TestResult {
        if !supported() {
            return Err(Fail::Skipped("CPU lacks PAN"));
        }
        if holds_everywhere(pan_set)? {
            Ok(())
        } else {
            Err("off on some CPU".into())
        }
    }
}
