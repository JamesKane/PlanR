//! arm64 page tables: 4 KiB granule, 48-bit VA, 2 MiB blocks; the kernel
//! half through `TTBR1_EL1`, the lower half through `TTBR0_EL1`.
//!
//! Every address space uses ASID 0 for now: kernel leaves are global and
//! user leaves are not (`nG`), so switching spaces drops ASID 0's entries
//! and keeps the kernel's. Invalidations use the inner-shareable forms,
//! which the hardware broadcasts to every CPU.

use core::arch::asm;

use bootinfo::arm64::{ATTR_DEVICE, ATTR_NORMAL, ATTR_NORMAL_NC};

use crate::port::mm::MapKind;
use crate::port::mmu::{self, Mmu};

const VALID: u64 = 1 << 0;
/// Table descriptor at levels 0-2; page descriptor at level 3.
const TABLE_OR_PAGE: u64 = 1 << 1;
/// AP[1]: EL0 may access the page.
const AP_EL0: u64 = 1 << 6;
const AP_READ_ONLY: u64 = 1 << 7;
const SH_INNER: u64 = 3 << 8;
const AF: u64 = 1 << 10;
/// Not global: tagged with the ASID, dropped when spaces switch.
const NG: u64 = 1 << 11;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ADDR: u64 = 0x0000_ffff_ffff_f000;

/// Nothing to do: 9boot programs `MAIR_EL1` with the attributes the kernel
/// uses, and the other CPUs copy the boot CPU's.
pub(crate) fn init_cpu() {}

/// The arm64 format.
#[derive(Debug)]
pub(crate) enum Native {}

impl Mmu for Native {
    const SHARED_ROOT: bool = false;
    const BROADCAST_FLUSH: bool = true;

    fn table(phys: u64, _user: bool) -> u64 {
        phys | VALID | TABLE_OR_PAGE
    }

    fn leaf(phys: u64, kind: MapKind, block: bool) -> u64 {
        let attrs = match kind {
            MapKind::Device => (ATTR_DEVICE << 2) | PXN | UXN,
            MapKind::NormalReadOnly => (ATTR_NORMAL << 2) | SH_INNER | AP_READ_ONLY | PXN | UXN,
            MapKind::NormalReadWrite => (ATTR_NORMAL << 2) | SH_INNER | PXN | UXN,
            MapKind::KernelText => (ATTR_NORMAL << 2) | SH_INNER | AP_READ_ONLY | UXN,
            MapKind::WriteCombining => (ATTR_NORMAL_NC << 2) | SH_INNER | PXN | UXN,
            MapKind::UserReadWrite => (ATTR_NORMAL << 2) | SH_INNER | AP_EL0 | NG | PXN | UXN,
            MapKind::UserReadOnly => (ATTR_NORMAL << 2) | SH_INNER | AP_EL0 | AP_READ_ONLY | NG | PXN | UXN,
            MapKind::UserText => (ATTR_NORMAL << 2) | SH_INNER | AP_EL0 | AP_READ_ONLY | NG | PXN,
        };
        phys | VALID | AF | attrs | if block { 0 } else { TABLE_OR_PAGE }
    }

    fn is_valid(e: u64) -> bool {
        e & VALID != 0
    }

    fn is_table(e: u64, _shift: u32) -> bool {
        e & TABLE_OR_PAGE != 0
    }

    fn address(e: u64) -> u64 {
        e & ADDR
    }

    fn publish() {
        // SAFETY: barriers only. The walker must observe the new entry
        // before any access through it: dsb orders the store, isb the
        // pipeline.
        unsafe { asm!("dsb ishst", "isb", options(nostack)) };
    }

    unsafe fn load(kernel: u64, user: u64) {
        // SAFETY: the caller's contract. The old tables' entries may stay in
        // the TLB until the flush, but they agree with the new ones on
        // everything in use.
        unsafe {
            asm!(
                "msr ttbr1_el1, {k}",
                "msr ttbr0_el1, {u}",
                "isb",
                "tlbi vmalle1",
                "dsb nsh",
                "isb",
                k = in(reg) kernel,
                u = in(reg) user,
                options(nostack),
            );
        }
    }

    unsafe fn activate(user: u64) {
        // SAFETY: the caller's contract; then drop ASID 0's (user) entries.
        unsafe {
            asm!(
                "msr ttbr0_el1, {u}",
                "isb",
                "tlbi aside1, xzr",
                "dsb nsh",
                "isb",
                u = in(reg) user,
                options(nostack),
            );
        }
    }

    fn flush_local(va: u64, pages: u64) {
        // SAFETY: TLB maintenance only.
        unsafe {
            if mmu::flush_all_instead(pages) {
                asm!("tlbi vmalle1", options(nostack));
            } else {
                for i in 0..pages {
                    asm!("tlbi vaae1, {}", in(reg) ((va >> 12) + i) & 0xfff_ffff_ffff, options(nostack));
                }
            }
            asm!("dsb nsh", "isb", options(nostack));
        }
    }

    fn flush_everywhere(va: u64, pages: u64) {
        // SAFETY: TLB maintenance only; the inner-shareable forms reach
        // every CPU, and the dsb waits until they all have.
        unsafe {
            asm!("dsb ishst", options(nostack));
            if mmu::flush_all_instead(pages) {
                asm!("tlbi vmalle1is", options(nostack));
            } else {
                for i in 0..pages {
                    asm!("tlbi vaae1is, {}", in(reg) ((va >> 12) + i) & 0xfff_ffff_ffff, options(nostack));
                }
            }
            asm!("dsb ish", "isb", options(nostack));
        }
    }
}

/// Makes code just written at `[va, va + len)` (through any mapping)
/// visible to instruction fetch: cleans the data cache to the point of
/// unification and drops every instruction cache.
pub(crate) fn sync_icache(va: u64, len: u64) {
    let ctr: u64;
    // SAFETY: reads the cache type register.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let line = 4 << ((ctr >> 16) & 0xf);
    let mut a = va & !(line - 1);
    while a < va + len {
        // SAFETY: cache maintenance on memory the caller owns.
        unsafe { asm!("dc cvau, {}", in(reg) a, options(nostack, preserves_flags)) };
        a += line;
    }
    // SAFETY: barriers and an instruction cache invalidate, broadcast.
    unsafe {
        asm!(
            "dsb ish",
            "ic ialluis",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags)
        );
    }
}
