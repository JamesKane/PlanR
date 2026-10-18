//! Early memory management: direct-map translation, a bump frame
//! allocator over the boot memory map, and an MMIO mapping window.
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

/// How a mapping should behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MapKind {
    /// Device registers: uncached, strongly ordered, never executable.
    Device,
    /// Ordinary memory outside the direct map (e.g. ACPI tables in reserved
    /// ranges): cacheable, read-only, never executable.
    NormalReadOnly,
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
static MEMORY: SpinLock<&'static [Region]> = SpinLock::new(&[]);

pub(crate) fn init(info: &'static BootInfo) {
    *FRAMES.lock() = Some(FrameAlloc {
        regions: info.memory(),
        index: 0,
        next: 0,
    });
    *MEMORY.lock() = info.memory();
}

/// Allocates one zeroed physical page.
pub(crate) fn alloc_frame() -> Option<u64> {
    let frame = FRAMES.lock().as_mut()?.alloc()?;
    // SAFETY: the frame is usable RAM, inside the direct map, and ours.
    unsafe { core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_BYTES) };
    Some(frame)
}

/// True if `[phys, phys+len)` is RAM covered by the direct map.
pub(crate) fn in_direct_map(phys: u64, len: u64) -> bool {
    let end = phys.saturating_add(len);
    MEMORY
        .lock()
        .iter()
        .any(|r| r.kind.is_ram() && r.start <= phys && end <= r.end())
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
    for i in 0..pages {
        arch::mmu::map_page(va + i * PAGE, first + i * PAGE, kind)?;
    }
    Some(va + (phys - first))
}
