//! arm64 page tables: 4 KiB granule, 48-bit VA, kernel half via `TTBR1_EL1`.

use core::arch::asm;

use bootinfo::{arm64::ATTR_DEVICE, arm64::ATTR_NORMAL, arm64::ATTR_NORMAL_NC, phys_to_virt};

use crate::port::mm::{self, MapKind};

const VALID: u64 = 1 << 0;
/// Table descriptor at levels 0-2; page descriptor at level 3.
const TABLE_OR_PAGE: u64 = 1 << 1;
const AP_READ_ONLY: u64 = 1 << 7;
const SH_INNER: u64 = 3 << 8;
const AF: u64 = 1 << 10;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ADDR: u64 = 0x0000_ffff_ffff_f000;

/// Nothing to do: 9boot programs `MAIR_EL1` with the attributes the kernel
/// uses, and the other CPUs copy the boot CPU's.
pub(crate) fn init_cpu() {}

fn table(phys: u64) -> *mut u64 {
    phys_to_virt(phys) as *mut u64
}

/// Maps one 4 KiB page in the kernel (TTBR1) address space.
pub(crate) fn map_page(va: u64, pa: u64, kind: MapKind) -> Option<()> {
    let ttbr1: u64;
    // SAFETY: reading TTBR1_EL1 has no side effects.
    unsafe { asm!("mrs {}, ttbr1_el1", out(reg) ttbr1, options(nomem, nostack)) };
    let mut t = table(ttbr1 & ADDR);
    for shift in [39, 30, 21] {
        let idx = ((va >> shift) & 511) as usize;
        // SAFETY: `t` is a live page-table page reached through the direct map.
        let e = unsafe { &mut *t.add(idx) };
        if *e & VALID == 0 {
            *e = mm::alloc_frame()? | VALID | TABLE_OR_PAGE;
        } else if *e & TABLE_OR_PAGE == 0 {
            return None; // already covered by a block mapping
        }
        t = table(*e & ADDR);
    }
    let attrs = match kind {
        MapKind::Device => (ATTR_DEVICE << 2) | PXN | UXN,
        MapKind::NormalReadOnly => (ATTR_NORMAL << 2) | SH_INNER | AP_READ_ONLY | PXN | UXN,
        MapKind::NormalReadWrite => (ATTR_NORMAL << 2) | SH_INNER | PXN | UXN,
        MapKind::WriteCombining => (ATTR_NORMAL_NC << 2) | SH_INNER | PXN | UXN,
    };
    // SAFETY: as above. The table walker must observe the new entry before
    // any access through it: dsb orders the store, isb the pipeline.
    unsafe {
        *t.add(((va >> 12) & 511) as usize) = pa | VALID | TABLE_OR_PAGE | AF | attrs;
        asm!("dsb ishst", "isb", options(nostack));
    }
    Some(())
}
