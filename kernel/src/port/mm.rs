//! Early memory management: direct-map translation, a bump frame
//! allocator over the boot memory map, an MMIO mapping window, and kernel
//! stacks with guard space below them.
//!
//! This is bring-up scaffolding. P2 replaces the frame allocator with the
//! real one and the boot page tables with the kernel's own.

use core::sync::atomic::{AtomicU64, Ordering};

use bootinfo::{BootInfo, Region, RegionKind, phys_to_virt};

use crate::arch;
use crate::port::lock::SpinLock;

pub(crate) const PAGE: u64 = 4096;
const PAGE_BYTES: usize = 4096;

/// Where MMIO and non-RAM mappings are placed.
const MMIO_BASE: u64 = 0xffff_c000_0000_0000;
const MMIO_LIMIT: u64 = 0xffff_c100_0000_0000;
/// Where kernel stacks are placed, one per [`STACK_SLOT`].
const STACK_BASE: u64 = 0xffff_c100_0000_0000;
const STACK_LIMIT: u64 = 0xffff_c200_0000_0000;
/// Each kernel stack has a slot of its own, aligned to its size. The stack
/// ends at the slot's midpoint and the rest of the slot is unmapped, so
/// at least 64 KiB of guard lies below every stack. On arm64, a stack
/// pointer inside a stack also has bit 16 clear, and one that has run off
/// the bottom into the slot below has it set. The trap entry tests that bit
/// without touching memory (see `arch/arm64/trap.rs`).
pub(crate) const STACK_SLOT: u64 = 128 * 1024;
/// The size of a kernel stack, and the most a slot holds. arm64 stacks
/// must be this size, so the bit-16 test catches any overflow.
pub(crate) const STACK_PAGES: u64 = STACK_SLOT / 2 / PAGE;

/// How a mapping should behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MapKind {
    /// Device registers: uncached, strongly ordered, never executable.
    Device,
    /// Ordinary memory outside the direct map (e.g. ACPI tables in reserved
    /// ranges): cacheable, read-only, never executable.
    NormalReadOnly,
    /// Ordinary memory outside the direct map (kernel stacks): cacheable,
    /// writable, never executable.
    NormalReadWrite,
    /// Memory that is written far more than read and must reach RAM or the
    /// device promptly (framebuffers, the log ring): uncached, but writes
    /// may be combined. Writable, never executable.
    WriteCombining,
    /// Ordinary memory in the lower half that user mode may read and write:
    /// cacheable, never executable. With SMAP or PAN on, the kernel reaches
    /// it only through [`arch::probe_user_read`].
    UserReadWrite,
}

#[derive(Debug)]
struct FrameAlloc {
    regions: &'static [Region],
    index: usize,
    next: u64,
}

impl FrameAlloc {
    /// Never hand out memory below 1 MiB: amd64 needs it for the AP
    /// start-up trampoline, and some firmware is touchy about it.
    const FLOOR: u64 = 0x10_0000;

    fn alloc(&mut self) -> Option<u64> {
        while let Some(r) = self.regions.get(self.index) {
            if r.kind == RegionKind::Usable {
                let start = self.next.max(r.start).max(Self::FLOOR).next_multiple_of(PAGE);
                if start + PAGE <= r.end() {
                    self.next = start + PAGE;
                    return Some(start);
                }
            }
            self.index += 1;
        }
        None
    }
}

static FRAMES: SpinLock<Option<FrameAlloc>> = SpinLock::new(None);
static MMIO_NEXT: AtomicU64 = AtomicU64::new(MMIO_BASE);
static STACK_NEXT: AtomicU64 = AtomicU64::new(STACK_BASE);
static MEMORY: SpinLock<&'static [Region]> = SpinLock::new(&[]);
/// Serializes page-table updates; held around each mapping's walk.
static MAP: SpinLock<()> = SpinLock::new(());
/// Where 9boot loaded the kernel image.
static KERNEL_PHYS: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    /// The first byte of the kernel image (the linker script).
    static __kernel_start: u8;
}

pub(crate) fn init(info: &'static BootInfo) {
    *FRAMES.lock() = Some(FrameAlloc {
        regions: info.memory(),
        index: 0,
        next: 0,
    });
    *MEMORY.lock() = info.memory();
    KERNEL_PHYS.store(info.kernel_phys, Ordering::Relaxed);
}

/// The physical address of `va`, which must be inside the kernel image.
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))] // arm64 AP start-up
pub(crate) fn kernel_phys(va: u64) -> u64 {
    va - (&raw const __kernel_start) as u64 + KERNEL_PHYS.load(Ordering::Relaxed)
}

/// Allocates one zeroed physical page.
pub(crate) fn alloc_frame() -> Option<u64> {
    let frame = FRAMES.lock().as_mut()?.alloc()?;
    // SAFETY: the frame is usable RAM, inside the direct map, and ours.
    unsafe { core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_BYTES) };
    Some(frame)
}

/// Finds `n` free, contiguous pages below 1 MiB (which the frame allocator
/// never hands out), avoiding page 0. They are RAM, so the identity and
/// direct maps cover them.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))] // the amd64 AP trampoline
pub(crate) fn low_pages(n: u64) -> Option<u64> {
    let want = n * PAGE;
    MEMORY
        .lock()
        .iter()
        .filter(|r| r.kind == RegionKind::Usable)
        .find_map(|r| {
            let start = r.start.max(PAGE).next_multiple_of(PAGE);
            let end = r.end().min(FrameAlloc::FLOOR);
            (start + want <= end).then_some(start)
        })
}

/// Allocates a kernel stack of `pages` pages (at most [`STACK_PAGES`]) in
/// a [`STACK_SLOT`] of its own, and returns the address of its top.
pub(crate) fn alloc_stack(pages: u64) -> Option<u64> {
    if pages > STACK_PAGES {
        return None;
    }
    let slot = STACK_NEXT.fetch_add(STACK_SLOT, Ordering::Relaxed);
    if slot + STACK_SLOT > STACK_LIMIT {
        return None;
    }
    let top = slot + STACK_SLOT / 2;
    let _map = MAP.lock();
    for i in 1..=pages {
        arch::mmu::map_page(top - i * PAGE, alloc_frame()?, MapKind::NormalReadWrite)?;
    }
    Some(top)
}

/// True if `[phys, phys+len)` is RAM covered by the direct map.
pub(crate) fn in_direct_map(phys: u64, len: u64) -> bool {
    let end = phys.saturating_add(len);
    MEMORY
        .lock()
        .iter()
        .any(|r| r.kind.is_ram() && r.start <= phys && end <= r.end())
}

/// True if any of `[phys, phys+len)` is RAM the frame allocator may hand
/// out.
pub(crate) fn overlaps_usable(phys: u64, len: u64) -> bool {
    let end = phys.saturating_add(len);
    MEMORY
        .lock()
        .iter()
        .any(|r| r.kind == RegionKind::Usable && r.start < end && phys < r.end())
}

/// Maps `len` bytes of physical memory at `phys` into the MMIO window and
/// returns the virtual address of `phys`.
pub(crate) fn map_phys(phys: u64, len: u64, kind: MapKind) -> Option<u64> {
    let first = phys & !(PAGE - 1);
    let pages = (phys + len - first).div_ceil(PAGE);
    let va = MMIO_NEXT.fetch_add(pages * PAGE, Ordering::Relaxed);
    if va + pages * PAGE > MMIO_LIMIT {
        return None;
    }
    let _map = MAP.lock();
    for i in 0..pages {
        arch::mmu::map_page(va + i * PAGE, first + i * PAGE, kind)?;
    }
    Some(va + (phys - first))
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use bootinfo::phys_to_virt;

    use super::{MAP, MapKind, PAGE, alloc_frame, alloc_stack};
    use crate::arch;
    use crate::port::ktest::TestResult;
    use crate::port::lock::SpinLock;

    /// Where [`user_page`] maps its page: near the top of the lower half,
    /// far above any RAM the identity map covers.
    const USER_PAGE: u64 = 0x0000_7fff_fff0_0000;
    /// The word at the start of the user page.
    pub(crate) const USER_WORD: u64 = 0x05e7_da7a_0000_beef;

    /// The address of a user page holding [`USER_WORD`], mapped on first
    /// use and kept: remapping it would need a TLB invalidation.
    pub(crate) fn user_page() -> Result<u64, &'static str> {
        static MAPPED: SpinLock<bool> = SpinLock::new(false);
        let mut mapped = MAPPED.lock();
        if !*mapped {
            let frame = alloc_frame().ok_or("no memory for a user page")?;
            // SAFETY: the frame is new, in the direct map and ours.
            unsafe { (phys_to_virt(frame) as *mut u64).write(USER_WORD) };
            let _map = MAP.lock();
            arch::mmu::map_page(USER_PAGE, frame, MapKind::UserReadWrite).ok_or("cannot map a user page")?;
            *mapped = true;
        }
        Ok(USER_PAGE)
    }

    /// A new stack is mapped and writable, and the page below it faults.
    pub(crate) fn stack_has_guard() -> TestResult {
        let top = alloc_stack(1).ok_or("no stack")?;
        let word = top - 8;
        // SAFETY: the stack's one page is mapped, writable and ours.
        unsafe { (word as *mut u64).write_volatile(0x5ac0_9a2d) };
        if arch::probe_read(word) != Some(0x5ac0_9a2d) {
            return Err("stack not readable".into());
        }
        match arch::probe_read(top - PAGE - 8) {
            None => Ok(()),
            Some(_) => Err("guard page is mapped".into()),
        }
    }
}
