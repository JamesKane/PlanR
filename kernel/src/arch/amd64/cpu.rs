//! amd64 instructions without a better home: CPUID, MSRs, the TSC.

use core::arch::asm;
use core::arch::x86_64::{__cpuid_count, CpuidResult};

pub(crate) fn cpuid(leaf: u32, sub: u32) -> CpuidResult {
    __cpuid_count(leaf, sub)
}

/// The highest basic (`0x0`) or extended (`0x8000_0000`) leaf.
pub(crate) fn max_leaf(base: u32) -> u32 {
    cpuid(base, 0).eax
}

pub(crate) fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: callers name architectural MSRs the CPU reports as present.
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    u64::from(hi) << 32 | u64::from(lo)
}

/// # Safety
///
/// The write must be valid for `msr` and must not break the kernel's
/// assumptions (paging, segmentation, interrupt routing).
#[allow(clippy::cast_possible_truncation)] // splitting into edx:eax
pub(crate) unsafe fn wrmsr(msr: u32, v: u64) {
    // SAFETY: the caller's.
    unsafe { asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack)) };
}

/// The time-stamp counter. `lfence` keeps it from being read before
/// earlier instructions complete.
pub(crate) fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: reads the TSC; no memory effects.
    unsafe { asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    u64::from(hi) << 32 | u64::from(lo)
}

/// True if the TSC runs at a constant rate in every power state
/// (CPUID `0x8000_0007` EDX bit 8).
pub(crate) fn invariant_tsc() -> bool {
    max_leaf(0x8000_0000) >= 0x8000_0007 && cpuid(0x8000_0007, 0).edx & (1 << 8) != 0
}

/// The TSC frequency from CPUID leaf `0x15` (crystal ratio), where the CPU
/// reports both the ratio and the crystal frequency. Intel only.
pub(crate) fn tsc_hz_from_cpuid() -> Option<u64> {
    if max_leaf(0) < 0x15 {
        return None;
    }
    let r = cpuid(0x15, 0);
    (r.eax != 0 && r.ebx != 0 && r.ecx != 0).then(|| u64::from(r.ecx) * u64::from(r.ebx) / u64::from(r.eax))
}
