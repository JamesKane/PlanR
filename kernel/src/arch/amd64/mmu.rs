//! amd64 page tables: 4-level, 4 KiB pages and 2 MiB blocks. One root
//! (CR3) covers both halves, so each address space's root carries the
//! kernel half's entries (`port/mmu.rs`).

use core::arch::asm;

use crate::port::mm::MapKind;
use crate::port::mmu::{self, Mmu};

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

fn read_cr3() -> u64 {
    let cr3: u64;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack)) };
    cr3
}

/// The amd64 format.
#[derive(Debug)]
pub(crate) enum Native {}

impl Mmu for Native {
    const SHARED_ROOT: bool = true;
    const BROADCAST_FLUSH: bool = false;

    fn table(phys: u64, user: bool) -> u64 {
        // A page is reachable from user mode only if every level allows
        // it; the leaves decide.
        phys | PRESENT | WRITABLE | if user { USER } else { 0 }
    }

    fn leaf(phys: u64, kind: MapKind, block: bool) -> u64 {
        let attrs = match kind {
            MapKind::Device => WRITABLE | CACHE_DISABLE | WRITE_THROUGH | NO_EXECUTE,
            MapKind::NormalReadOnly => NO_EXECUTE,
            MapKind::NormalReadWrite => WRITABLE | NO_EXECUTE,
            MapKind::KernelText => 0,
            MapKind::WriteCombining => WRITABLE | WRITE_THROUGH | NO_EXECUTE,
            MapKind::UserReadWrite => WRITABLE | USER | NO_EXECUTE,
            MapKind::UserText => USER,
        };
        phys | PRESENT | attrs | if block { HUGE } else { 0 }
    }

    fn is_valid(e: u64) -> bool {
        e & PRESENT != 0
    }

    fn is_table(e: u64, shift: u32) -> bool {
        shift == 39 || e & HUGE == 0
    }

    fn address(e: u64) -> u64 {
        e & ADDR
    }

    fn publish() {}

    unsafe fn load(kernel: u64, _user: u64) {
        // SAFETY: the caller's contract. Writing CR3 flushes the TLB.
        unsafe { asm!("mov cr3, {}", in(reg) kernel, options(nostack)) };
    }

    unsafe fn activate(user: u64) {
        if read_cr3() & ADDR != user {
            // SAFETY: the caller's contract.
            unsafe { asm!("mov cr3, {}", in(reg) user, options(nostack)) };
        }
    }

    fn flush_local(va: u64, pages: u64) {
        if mmu::flush_all_instead(pages) {
            // SAFETY: reloading CR3 flushes every non-global translation,
            // and the kernel uses none that are global.
            unsafe { asm!("mov cr3, {}", in(reg) read_cr3(), options(nostack)) };
            return;
        }
        for i in 0..pages {
            // SAFETY: invlpg only drops a translation.
            unsafe { asm!("invlpg [{}]", in(reg) va + i * 4096, options(nostack)) };
        }
    }

    fn flush_everywhere(va: u64, pages: u64) {
        Self::flush_local(va, pages);
    }
}

/// Nothing to do: instruction fetch sees stores on amd64.
pub(crate) fn sync_icache(_va: u64, _len: u64) {}
