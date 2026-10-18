//! CPU protections, each turned on where CPUID reports it: SMEP (the
//! kernel cannot execute user pages), SMAP (nor read or write them outside
//! a `stac`/`clac` window) and UMIP (user mode cannot read the descriptor
//! table registers). NX and `CR0.WP` are on from 9boot.

use core::sync::atomic::{AtomicBool, Ordering};

use super::cpu;
use crate::port::ktest::{Fail, TestResult};
use crate::println;

const CR4_UMIP: u64 = 1 << 11;
const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;

/// True once SMAP is on. `stac` and `clac` fault on a CPU without SMAP, so
/// the trap entry and the user-access path read this byte first.
pub(super) static SMAP: AtomicBool = AtomicBool::new(false);

/// The protection bits of CR4 this CPU supports (CPUID leaf 7).
fn supported() -> u64 {
    if cpu::max_leaf(0) < 7 {
        return 0;
    }
    let r = cpu::cpuid(7, 0);
    let mut bits = 0;
    if r.ebx & (1 << 7) != 0 {
        bits |= CR4_SMEP;
    }
    if r.ebx & (1 << 20) != 0 {
        bits |= CR4_SMAP;
    }
    if r.ecx & (1 << 2) != 0 {
        bits |= CR4_UMIP;
    }
    bits
}

/// Turns on the calling CPU's protections. Every CPU calls it; the
/// application processors also inherit the boot CPU's CR4.
pub(crate) fn init_cpu() {
    let bits = supported();
    // SAFETY: SMEP and SMAP fault only on kernel use of user pages, and
    // nothing in the kernel touches one outside the user-access path (9boot
    // maps none). UMIP affects only user mode.
    unsafe { cpu::write_cr4(cpu::read_cr4() | bits) };
    if bits & CR4_SMAP != 0 {
        SMAP.store(true, Ordering::Relaxed);
    }
}

/// True if a plain kernel access to a user page faults.
pub(crate) fn user_access_guarded() -> bool {
    SMAP.load(Ordering::Relaxed)
}

/// Prints which protections are on.
pub(crate) fn report() {
    let cr4 = cpu::read_cr4();
    let state = |bit| if cr4 & bit != 0 { "on" } else { "unsupported" };
    println!(
        "cpu: SMEP {}, SMAP {}, UMIP {}",
        state(CR4_SMEP),
        state(CR4_SMAP),
        state(CR4_UMIP)
    );
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use super::{CR4_SMAP, CR4_SMEP, CR4_UMIP, Fail, TestResult, cpu, supported};
    use crate::port::cpu::ktests::holds_everywhere;

    fn cr4_has<const BIT: u64>() -> bool {
        cpu::read_cr4() & BIT != 0
    }

    /// CR4 has `BIT` set on every online CPU.
    fn on_every_cpu<const BIT: u64>(lacks: &'static str) -> TestResult {
        if supported() & BIT == 0 {
            return Err(Fail::Skipped(lacks));
        }
        if holds_everywhere(cr4_has::<BIT>)? {
            Ok(())
        } else {
            Err("off on some CPU".into())
        }
    }

    pub(crate) fn smep() -> TestResult {
        on_every_cpu::<CR4_SMEP>("CPU lacks SMEP")
    }

    pub(crate) fn smap() -> TestResult {
        on_every_cpu::<CR4_SMAP>("CPU lacks SMAP")
    }

    pub(crate) fn umip() -> TestResult {
        on_every_cpu::<CR4_UMIP>("CPU lacks UMIP")
    }
}
