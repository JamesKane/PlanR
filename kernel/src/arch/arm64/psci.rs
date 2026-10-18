//! PSCI firmware calls, through the conduit the FADT names.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

use acpi::PsciConduit;

use crate::port::acpi::System;

const NONE: u8 = 0;
const SMC: u8 = 1;
const HVC: u8 = 2;
static CONDUIT: AtomicU8 = AtomicU8::new(NONE);

/// Records how to reach PSCI firmware, from the FADT.
pub(crate) fn init(acpi: Option<&System>) {
    let c = match acpi.and_then(|s| s.tables.fadt()).and_then(|f| f.psci()) {
        Some(PsciConduit::Smc) => SMC,
        Some(PsciConduit::Hvc) => HVC,
        None => NONE,
    };
    CONDUIT.store(c, Ordering::Relaxed);
}

pub(crate) fn available() -> bool {
    CONDUIT.load(Ordering::Relaxed) != NONE
}

/// An SMC Calling Convention call through instruction `$insn`: x0-x3
/// carry the function and arguments, x0 the result, and x4-x17 may be
/// clobbered.
macro_rules! smccc {
    ($insn:literal, $r:ident, $a1:expr, $a2:expr, $a3:expr) => {
        asm!(
            $insn,
            inout("x0") $r,
            inout("x1") $a1 => _,
            inout("x2") $a2 => _,
            inout("x3") $a3 => _,
            out("x4") _, out("x5") _, out("x6") _, out("x7") _,
            out("x8") _, out("x9") _, out("x10") _, out("x11") _,
            out("x12") _, out("x13") _, out("x14") _, out("x15") _,
            out("x16") _, out("x17") _,
            options(nostack),
        )
    };
}

/// Calls PSCI function `f` with three arguments and returns its status:
/// 0 for success, negative for an error; -1 (`NOT_SUPPORTED`) without PSCI.
#[allow(clippy::cast_possible_wrap)] // PSCI returns a signed status in x0
pub(crate) fn call(f: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let mut r = f;
    match CONDUIT.load(Ordering::Relaxed) {
        // SAFETY: a PSCI call; the firmware preserves everything else.
        SMC => unsafe { smccc!("smc #0", r, a1, a2, a3) },
        // SAFETY: as above.
        HVC => unsafe { smccc!("hvc #0", r, a1, a2, a3) },
        _ => return -1,
    }
    r as i64
}
