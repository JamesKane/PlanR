//! amd64 page tables: 4-level, 4 KiB pages.

use core::arch::asm;

use bootinfo::phys_to_virt;

use crate::port::mm::{self, MapKind};

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const ADDR: u64 = 0x000f_ffff_ffff_f000;

fn table(phys: u64) -> *mut u64 {
    phys_to_virt(phys) as *mut u64
}

/// Maps one 4 KiB page in the current address space.
pub(crate) fn map_page(va: u64, pa: u64, kind: MapKind) -> Option<()> {
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
        t = table(*e & ADDR);
    }
    let leaf = match kind {
        MapKind::Device => PRESENT | WRITABLE | CACHE_DISABLE | WRITE_THROUGH | NO_EXECUTE,
        MapKind::NormalReadOnly => PRESENT | NO_EXECUTE,
        MapKind::NormalReadWrite => PRESENT | WRITABLE | NO_EXECUTE,
    };
    // SAFETY: as above; a not-present entry is never cached in the TLB, so
    // no invalidation is needed for a fresh mapping.
    unsafe { *t.add(((va >> 12) & 511) as usize) = pa | leaf };
    Some(())
}
