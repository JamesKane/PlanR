//! Memory management: the physical frame allocator (`lib/frames`, a
//! buddy allocator over the boot memory map's free RAM), an MMIO mapping
//! window, and kernel stacks with guard space below them.
//!
//! The page tables are the kernel's own (`port/mmu.rs`), built in
//! [`init`].

use core::sync::atomic::{AtomicU64, Ordering};

use bootinfo::{BootInfo, HHDM_BASE, Region, RegionKind, phys_to_virt};
use frames::Frames;

use crate::port::lock::SpinLock;
use crate::port::mmu::{self, MapError};

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
const STACK_PAGES_USIZE: usize = 128 * 1024 / 2 / PAGE_BYTES;
const _: () = assert!(STACK_PAGES_USIZE as u64 == STACK_PAGES);

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
    /// The kernel's code: cacheable, read-only, executable.
    KernelText,
    /// Ordinary memory in the lower half that user mode may read and write:
    /// cacheable, never executable. With SMAP or PAN on, the kernel reaches
    /// it only through [`crate::arch::probe_user_read`].
    UserReadWrite,
    /// User code: cacheable, read-only, executable in user mode only.
    UserText,
}

impl MapKind {
    /// True for the kinds that belong in the lower (user) half.
    pub(crate) fn is_user(self) -> bool {
        matches!(self, MapKind::UserReadWrite | MapKind::UserText)
    }
}

/// Never hand out memory below 1 MiB: amd64 needs it for the AP start-up
/// trampoline, and some firmware is touchy about it.
const FLOOR: u64 = 0x10_0000;

static FRAMES: SpinLock<Frames> = SpinLock::new(Frames::new(HHDM_BASE));
static MMIO_NEXT: AtomicU64 = AtomicU64::new(MMIO_BASE);
static STACK_NEXT: AtomicU64 = AtomicU64::new(STACK_BASE);
static MEMORY: SpinLock<&'static [Region]> = SpinLock::new(&[]);
/// Where 9boot loaded the kernel image.
static KERNEL_PHYS: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    /// The first byte of the kernel image (the linker script).
    static __kernel_start: u8;
}

/// Takes over the boot memory map's free RAM and switches to the kernel's
/// own page tables.
pub(crate) fn init(info: &'static BootInfo) -> Result<(), MapError> {
    let mut frames = FRAMES.lock();
    for r in info.memory().iter().filter(|r| r.kind == RegionKind::Usable) {
        // SAFETY: usable RAM is free once the kernel runs, and 9boot's
        // direct map covers it at `HHDM_BASE + phys`.
        unsafe { frames.add(r.start.max(FLOOR), r.end()) };
    }
    drop(frames);
    *MEMORY.lock() = info.memory();
    KERNEL_PHYS.store(info.kernel_phys, Ordering::Relaxed);
    mmu::init(info)
}

/// Free and total pages, and the number of arenas they are in.
pub(crate) fn frame_counts() -> (u64, u64, usize) {
    let f = FRAMES.lock();
    (f.free_pages(), f.total_pages(), f.arenas())
}

/// The physical address of `va`, which must be inside the kernel image.
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))] // arm64 AP start-up
pub(crate) fn kernel_phys(va: u64) -> u64 {
    va - (&raw const __kernel_start) as u64 + KERNEL_PHYS.load(Ordering::Relaxed)
}

/// Allocates one zeroed physical page.
pub(crate) fn alloc_frame() -> Option<u64> {
    alloc_frames(0)
}

/// Allocates `2^order` zeroed, contiguous physical pages, aligned to their
/// size.
pub(crate) fn alloc_frames(order: usize) -> Option<u64> {
    let frame = FRAMES.lock().alloc(order)?;
    // SAFETY: the block is usable RAM, inside the direct map, and ours.
    unsafe { core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_BYTES << order) };
    Some(frame)
}

/// Frees what [`alloc_frames`] returned. Freeing anything else is a kernel
/// bug, and panics.
///
/// # Safety
///
/// Nothing may use the pages afterwards, through any mapping.
pub(crate) unsafe fn free_frames(phys: u64, order: usize) {
    // SAFETY: the caller's contract.
    if let Err(e) = unsafe { FRAMES.lock().free(phys, order) } {
        panic!("mm: bad free of {phys:#x} order {order}: {e:?}");
    }
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
            let end = r.end().min(FLOOR);
            (start + want <= end).then_some(start)
        })
}

/// Allocates a kernel stack of `pages` pages (at most [`STACK_PAGES`]) in
/// a [`STACK_SLOT`] of its own, and returns the address of its top. The
/// stack is never freed; [`Stack`] is one that is.
pub(crate) fn alloc_stack(pages: u64) -> Option<u64> {
    if pages > STACK_PAGES {
        return None;
    }
    let reused = FREE_SLOTS.lock().pop();
    let slot = if let Some(slot) = reused {
        slot
    } else {
        let slot = STACK_NEXT.fetch_add(STACK_SLOT, Ordering::Relaxed);
        if slot + STACK_SLOT > STACK_LIMIT {
            return None;
        }
        slot
    };
    let top = slot + STACK_SLOT / 2;
    let mut k = mmu::kernel();
    for i in 1..=pages {
        let mapped = alloc_frame().map(|f| k.map(top - i * PAGE, f, MapKind::NormalReadWrite));
        if mapped != Some(Ok(())) {
            drop(k);
            // SAFETY: nothing has seen the stack; the pages mapped so far
            // are ours.
            unsafe { free_stack(top, i - 1) };
            return None;
        }
    }
    Some(top)
}

/// Stack slots given back by [`free_stack`], for reuse.
static FREE_SLOTS: SpinLock<alloc::vec::Vec<u64>> = SpinLock::new(alloc::vec::Vec::new());

/// Unmaps the `pages` pages below `top`, flushes them from every CPU, and
/// frees them and their slot. Must be called with no spin lock held
/// (`port/mmu.rs`).
///
/// # Safety
///
/// [`alloc_stack`] returned `top` for `pages` pages, and nothing runs on
/// or points into the stack any more.
pub(crate) unsafe fn free_stack(top: u64, pages: u64) {
    let mut frames = [0; STACK_PAGES_USIZE];
    let mut k = mmu::kernel();
    for (i, f) in (1..=pages).zip(frames.iter_mut()) {
        *f = k.unmap(top - i * PAGE).unwrap_or(0);
    }
    drop(k);
    mmu::shootdown(u64::MAX, top - pages * PAGE, pages);
    for f in frames.into_iter().filter(|&f| f != 0) {
        // SAFETY: unmapped and flushed everywhere.
        unsafe { free_frames(f, 0) };
    }
    FREE_SLOTS.lock().push(top - STACK_SLOT / 2);
}

/// A kernel stack of [`STACK_PAGES`], freed when dropped.
#[derive(Debug)]
pub(crate) struct Stack {
    top: u64,
}

impl Stack {
    pub(crate) fn new() -> Option<Stack> {
        Some(Stack {
            top: alloc_stack(STACK_PAGES)?,
        })
    }

    /// The address just above the stack.
    pub(crate) fn top(&self) -> u64 {
        self.top
    }
}

impl Drop for Stack {
    /// Must happen with no spin lock held, and off this stack.
    fn drop(&mut self) {
        // SAFETY: the stack is ours, and whoever drops it is done with it.
        unsafe { free_stack(self.top, STACK_PAGES) };
    }
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
    let mut k = mmu::kernel();
    for i in 0..pages {
        k.map(va + i * PAGE, first + i * PAGE, kind).ok()?;
    }
    Some(va + (phys - first))
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use bootinfo::phys_to_virt;

    use super::{MapKind, PAGE, Stack, alloc_frame, alloc_frames, alloc_stack, frame_counts, free_frames};
    use crate::arch;
    use crate::port::ktest::TestResult;
    use crate::port::lock::SpinLock;
    use crate::port::mmu::KERNEL_SPACE;

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
            KERNEL_SPACE
                .map(USER_PAGE, frame, MapKind::UserReadWrite)
                .map_err(|_| "cannot map a user page")?;
            *mapped = true;
        }
        Ok(USER_PAGE)
    }

    /// A freed block is counted back, and a block of any order comes
    /// zeroed and aligned to its size.
    pub(crate) fn frames_free_and_align() -> TestResult {
        const ORDER: usize = 4;
        let before = frame_counts().0;
        let block = alloc_frames(ORDER).ok_or("no memory for a block")?;
        if block % (PAGE << ORDER) != 0 {
            return Err("block not aligned to its size".into());
        }
        let bytes = super::PAGE_BYTES << ORDER;
        // SAFETY: the block is ours and in the direct map.
        let zeroed = unsafe { core::slice::from_raw_parts(phys_to_virt(block) as *const u8, bytes) };
        if zeroed.iter().any(|&b| b != 0) {
            return Err("block not zeroed".into());
        }
        // Dirty it, so the next allocation of it shows the zeroing.
        // SAFETY: as above.
        unsafe { core::ptr::write_bytes(phys_to_virt(block) as *mut u8, 0xa5, bytes) };
        // SAFETY: nothing else has the block.
        unsafe { free_frames(block, ORDER) };
        if frame_counts().0 != before {
            return Err("free count not restored".into());
        }
        Ok(())
    }

    /// A freed stack's slot and frames are reused, and its pages fault.
    pub(crate) fn stacks_free() -> TestResult {
        // Once first, so the free-slot list has room and is not counted.
        drop(Stack::new().ok_or("no stack")?);
        let before = frame_counts().0;
        let top = Stack::new().ok_or("no stack")?.top();
        if frame_counts().0 != before {
            return Err("stack frames not freed".into());
        }
        if arch::probe_read(top - 8).is_some() {
            return Err("freed stack still mapped".into());
        }
        let again = Stack::new().ok_or("no stack")?;
        if again.top() != top {
            return Err("slot not reused".into());
        }
        Ok(())
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
