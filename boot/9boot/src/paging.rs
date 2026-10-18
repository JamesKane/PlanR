//! Builds the boot page tables (ADR 0010): an identity map and the direct
//! map of RAM, plus the kernel image with per-segment permissions.
//!
//! The walk is generic; [`Format`] supplies each architecture's descriptor
//! encoding. Both use 4 KiB granules, 4 levels and 48-bit addresses.

use uefi::{BootServices, MemoryType};

const PAGE: u64 = 4096;
const PAGE_BYTES: usize = 4096;
const BLOCK: u64 = 2 << 20;

/// Access rights of a mapping. W^X: there is no writable-executable kind
/// except for the temporary identity map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // every mapping is readable; the names say what else
pub(crate) enum Perm {
    ReadOnly,
    ReadWrite,
    ReadExec,
    /// Identity map only: lets the loader keep running across the switch.
    ReadWriteExec,
    /// Read-write, not executable, uncached: the framebuffer, so the
    /// kernel can mark its progress before it maps anything itself.
    Uncached,
}

/// Leaf size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Size {
    Page4K,
    Block2M,
}

/// An architecture's descriptor encoding.
pub(crate) trait Format {
    /// Number of translation roots: amd64 has one; arm64 has TTBR0 (lower
    /// half) and TTBR1 (upper half).
    const ROOTS: usize;
    fn root_index(va: u64) -> usize;
    fn table(phys: u64) -> u64;
    fn leaf(phys: u64, perm: Perm, size: Size) -> u64;
    fn is_valid(e: u64) -> bool;
    /// True for a pointer to a next-level table (as opposed to a leaf).
    fn is_table(e: u64, level_shift: u32) -> bool;
    fn address(e: u64) -> u64;
}

#[derive(Debug)]
pub(crate) struct Tables<'a, F: Format> {
    bs: BootServices<'a>,
    pub(crate) roots: [u64; 2],
    pub(crate) pages_used: usize,
    _f: core::marker::PhantomData<F>,
}

impl<'a, F: Format> Tables<'a, F> {
    pub(crate) fn new(bs: BootServices<'a>) -> uefi::Result<Self> {
        let mut t = Tables {
            bs,
            roots: [0; 2],
            pages_used: 0,
            _f: core::marker::PhantomData,
        };
        for i in 0..F::ROOTS {
            t.roots[i] = t.frame()?;
        }
        Ok(t)
    }

    /// A zeroed page for a table, tagged so the kernel knows it is in use.
    fn frame(&mut self) -> uefi::Result<u64> {
        let p = self
            .bs
            .allocate_pages(MemoryType::Other(bootinfo::uefi_type::BOOT), 1)?;
        // SAFETY: UEFI identity-maps memory; the page is ours.
        unsafe { core::ptr::write_bytes(p as *mut u8, 0, PAGE_BYTES) };
        self.pages_used += 1;
        Ok(p)
    }

    /// Returns the table entry for `va` at the level with `stop_shift`,
    /// creating intermediate tables. `None` if a leaf is in the way.
    fn entry(&mut self, va: u64, stop_shift: u32) -> uefi::Result<Option<*mut u64>> {
        let mut table = self.roots[F::root_index(va)];
        for shift in [39u32, 30, 21, 12] {
            let slot = (table as *mut u64).wrapping_add(((va >> shift) & 511) as usize);
            if shift == stop_shift {
                return Ok(Some(slot));
            }
            // SAFETY: `table` is one of our identity-mapped table pages.
            let e = unsafe { *slot };
            if !F::is_valid(e) {
                let t = self.frame()?;
                // SAFETY: as above.
                unsafe { *slot = F::table(t) };
                table = t;
            } else if F::is_table(e, shift) {
                table = F::address(e);
            } else {
                return Ok(None);
            }
        }
        unreachable!("stop_shift is 21 or 12")
    }

    /// Maps `[va, va+len)` to `[pa, pa+len)`; all page aligned. Uses 2 MiB
    /// blocks where alignment and the existing tables allow.
    pub(crate) fn map(&mut self, mut va: u64, mut pa: u64, len: u64, perm: Perm) -> uefi::Result<()> {
        let end = va + len;
        while va < end {
            if va.is_multiple_of(BLOCK) && pa.is_multiple_of(BLOCK) && end - va >= BLOCK {
                let slot = self.entry(va, 21)?;
                // SAFETY: `entry` returns a valid slot in one of our tables.
                if let Some(slot) = slot.filter(|&s| !F::is_valid(unsafe { *s })) {
                    // SAFETY: as above.
                    unsafe { *slot = F::leaf(pa, perm, Size::Block2M) };
                    va += BLOCK;
                    pa += BLOCK;
                    continue;
                }
            }
            let slot = self.entry(va, 12)?.ok_or(uefi::Status::INVALID_PARAMETER)?;
            // SAFETY: as above.
            unsafe { *slot = F::leaf(pa, perm, Size::Page4K) };
            va += PAGE;
            pa += PAGE;
        }
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) use amd64::Amd64 as Native;
#[cfg(target_arch = "aarch64")]
pub(crate) use arm64::Arm64 as Native;

#[cfg(target_arch = "x86_64")]
mod amd64 {
    use super::{Format, Perm, Size};

    const PRESENT: u64 = 1 << 0;
    const WRITABLE: u64 = 1 << 1;
    /// PWT and PCD: with the default PAT, uncached.
    const WRITE_THROUGH: u64 = 1 << 3;
    const CACHE_DISABLE: u64 = 1 << 4;
    const HUGE: u64 = 1 << 7;
    const NO_EXECUTE: u64 = 1 << 63;
    const ADDR: u64 = 0x000f_ffff_ffff_f000;

    #[derive(Debug)]
    pub(crate) enum Amd64 {}

    impl Format for Amd64 {
        const ROOTS: usize = 1;
        fn root_index(_va: u64) -> usize {
            0
        }
        fn table(phys: u64) -> u64 {
            phys | PRESENT | WRITABLE
        }
        fn leaf(phys: u64, perm: Perm, size: Size) -> u64 {
            let mut e = phys | PRESENT;
            if matches!(perm, Perm::ReadWrite | Perm::ReadWriteExec | Perm::Uncached) {
                e |= WRITABLE;
            }
            if matches!(perm, Perm::ReadOnly | Perm::ReadWrite | Perm::Uncached) {
                e |= NO_EXECUTE;
            }
            if perm == Perm::Uncached {
                e |= CACHE_DISABLE | WRITE_THROUGH;
            }
            if size == Size::Block2M {
                e |= HUGE;
            }
            e
        }
        fn is_valid(e: u64) -> bool {
            e & PRESENT != 0
        }
        fn is_table(e: u64, level_shift: u32) -> bool {
            level_shift == 39 || e & HUGE == 0
        }
        fn address(e: u64) -> u64 {
            e & ADDR
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod arm64 {
    use super::{Format, Perm, Size};
    use bootinfo::arm64::{ATTR_NORMAL, ATTR_NORMAL_NC};

    const VALID: u64 = 1 << 0;
    const TABLE_OR_PAGE: u64 = 1 << 1;
    const AP_READ_ONLY: u64 = 1 << 7;
    const SH_INNER: u64 = 3 << 8;
    const AF: u64 = 1 << 10;
    const PXN: u64 = 1 << 53;
    const UXN: u64 = 1 << 54;
    const ADDR: u64 = 0x0000_ffff_ffff_f000;

    #[derive(Debug)]
    pub(crate) enum Arm64 {}

    impl Format for Arm64 {
        const ROOTS: usize = 2;
        fn root_index(va: u64) -> usize {
            usize::from(va >> 63 == 1)
        }
        fn table(phys: u64) -> u64 {
            phys | VALID | TABLE_OR_PAGE
        }
        fn leaf(phys: u64, perm: Perm, size: Size) -> u64 {
            let attr = if perm == Perm::Uncached {
                ATTR_NORMAL_NC
            } else {
                ATTR_NORMAL
            };
            let mut e = phys | VALID | AF | SH_INNER | (attr << 2) | UXN;
            if size == Size::Page4K {
                e |= TABLE_OR_PAGE;
            }
            if matches!(perm, Perm::ReadOnly | Perm::ReadExec) {
                e |= AP_READ_ONLY;
            }
            if matches!(perm, Perm::ReadOnly | Perm::ReadWrite | Perm::Uncached) {
                e |= PXN;
            }
            e
        }
        fn is_valid(e: u64) -> bool {
            e & VALID != 0
        }
        fn is_table(e: u64, _level_shift: u32) -> bool {
            e & TABLE_OR_PAGE != 0
        }
        fn address(e: u64) -> u64 {
            e & ADDR
        }
    }
}
