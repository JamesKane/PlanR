//! amd64 page tables: 4-level, 4 KiB pages.

use core::arch::asm;

use bootinfo::phys_to_virt;

use crate::port::mm::{self, MapKind};

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const ADDR: u64 = 0x000f_ffff_ffff_f000;

const IA32_PAT: u32 = 0x277;
/// The page attribute table: the power-on types (WB, WT, UC-, UC, twice)
/// except that entry 1, which `WRITE_THROUGH` alone selects, is
/// write-combining instead of write-through.
const PAT: u64 = 0x0007_0106_0007_0106;

/// Programs this CPU's page attribute table; every CPU must agree.
pub(crate) fn init_cpu() {
    // SAFETY: nothing maps with `WRITE_THROUGH` alone before this (9boot
    // never sets it), so no translation uses the entry that changes.
    unsafe { super::cpu::wrmsr(IA32_PAT, PAT) };
}

fn table(phys: u64) -> *mut u64 {
    phys_to_virt(phys) as *mut u64
}

/// Maps one 4 KiB page in the current address space. User pages go in
/// the lower half only.
pub(crate) fn map_page(va: u64, pa: u64, kind: MapKind) -> Option<()> {
    let user = kind == MapKind::UserReadWrite;
    if user && va >> 63 != 0 {
        return None;
    }
    let cr3: u64;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack)) };
    let mut t = table(cr3 & ADDR);
    for shift in [39, 30, 21] {
        let idx = ((va >> shift) & 511) as usize;
        // SAFETY: `t` is a live page-table page reached through the direct map.
        let e = unsafe { &mut *t.add(idx) };
        if *e & PRESENT == 0 {
            *e = mm::alloc_frame()? | PRESENT | WRITABLE;
        } else if *e & HUGE != 0 {
            return None; // already covered by a large page
        }
        // A page is reachable from user mode only if every level allows it.
        if user {
            *e |= USER;
        }
        t = table(*e & ADDR);
    }
    let leaf = match kind {
        MapKind::Device => PRESENT | WRITABLE | CACHE_DISABLE | WRITE_THROUGH | NO_EXECUTE,
        MapKind::NormalReadOnly => PRESENT | NO_EXECUTE,
        MapKind::NormalReadWrite => PRESENT | WRITABLE | NO_EXECUTE,
        MapKind::WriteCombining => PRESENT | WRITABLE | WRITE_THROUGH | NO_EXECUTE,
        MapKind::UserReadWrite => PRESENT | WRITABLE | USER | NO_EXECUTE,
    };
    // SAFETY: as above; a not-present entry is never cached in the TLB, so
    // no invalidation is needed for a fresh mapping.
    unsafe { *t.add(((va >> 12) & 511) as usize) = pa | leaf };
    Some(())
}
