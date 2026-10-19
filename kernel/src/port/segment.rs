//! A process's memory: an address space ([`Space`]) and the segments that
//! say what belongs in it.
//!
//! **Segments** are Plan 9's: text, data, bss and stack, each a page-aligned
//! range of the user half that owns the frames mapped in it. A segment may
//! have an *image*, file bytes that fill its pages (the program's text and
//! data, from the boot archive); the rest of every page is zero.
//!
//! **Demand paging.** Nothing is mapped until it is touched. A fault on an
//! unmapped page of a segment ([`fault`]) fills a new frame and maps it,
//! and the access is retried. The architecture's trap handler calls in for
//! faults from user mode, and for faults the kernel takes in its own user
//! copies (`port/uaccess.rs`) before it takes their fixup, so a system call
//! on a buffer not yet touched works.
//!
//! **Freeing.** A segment's frames go back when it is dropped, with its
//! [`Memory`], once no CPU runs in the space: switching spaces drops the
//! old one's translations on both architectures, so no TLB still holds
//! them. Shrinking the bss (`brk`) unmaps and flushes its pages first.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use bootinfo::phys_to_virt;

use crate::arch;
use crate::port::lock::SpinLock;
use crate::port::mm::{self, MapKind, PAGE};
use crate::port::mmu::Space;
use crate::port::proc;
use crate::port::uaccess::{self, BadAddress};

/// What a segment holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Code and read-only data: readable and executable, never writable.
    Text,
    /// Initialized data: read-write.
    Data,
    /// Zeroed data, which `brk` grows and shrinks: read-write.
    Bss,
    /// The stack: read-write.
    Stack,
}

impl Kind {
    fn map_kind(self) -> MapKind {
        match self {
            Kind::Text => MapKind::UserText,
            Kind::Data | Kind::Bss | Kind::Stack => MapKind::UserReadWrite,
        }
    }

    fn allows(self, access: Access) -> bool {
        match access {
            Access::Read => true,
            Access::Write => self != Kind::Text,
            Access::Exec => self == Kind::Text,
        }
    }
}

/// What a faulting access tried to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
    Exec,
}

/// A range of the user half and the frames mapped in it.
#[derive(Debug)]
pub(crate) struct Segment {
    kind: Kind,
    base: u64,
    top: u64,
    /// Bytes that belong at `image_at`, filling the pages they overlap.
    image: &'static [u8],
    image_at: u64,
    /// Mapped pages, by address, and their frames.
    pages: BTreeMap<u64, u64>,
}

impl Segment {
    /// A segment over `[base, top)`, both page aligned, filled from
    /// `image` at `image_at` (empty for none).
    pub(crate) fn new(kind: Kind, base: u64, top: u64, image: &'static [u8], image_at: u64) -> Segment {
        assert!(
            base.is_multiple_of(PAGE) && top.is_multiple_of(PAGE) && base <= top,
            "segment not page aligned"
        );
        Segment {
            kind,
            base,
            top,
            image,
            image_at,
            pages: BTreeMap::new(),
        }
    }

    fn contains(&self, va: u64) -> bool {
        self.base <= va && va < self.top
    }

    /// A new frame holding the page at `page`: the image where it overlaps,
    /// zeros elsewhere.
    fn fill(&self, page: u64) -> Option<u64> {
        let frame = mm::alloc_frame()?;
        let (start, end) = (
            page.max(self.image_at),
            (page + PAGE).min(self.image_at + self.image.len() as u64),
        );
        if start < end {
            #[allow(clippy::cast_possible_truncation)] // inside the image
            let from = &self.image[(start - self.image_at) as usize..(end - self.image_at) as usize];
            // SAFETY: the frame is new, ours, and in the direct map; the
            // bytes fit inside it.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    from.as_ptr(),
                    (phys_to_virt(frame) + (start - page)) as *mut u8,
                    from.len(),
                );
            }
        }
        if self.kind == Kind::Text {
            arch::mmu::sync_icache(phys_to_virt(frame), PAGE);
        }
        Some(frame)
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        for &frame in self.pages.values() {
            // SAFETY: a segment is dropped with its memory, whose space no
            // CPU runs in, so no mapping or translation of the frame is left
            // (see the module comment).
            unsafe { mm::free_frames(frame, 0) };
        }
    }
}

/// Why a fault was not resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultError {
    /// No segment covers the address.
    NoSegment,
    /// The segment does not allow the access.
    Denied,
    NoMemory,
}

/// Why a segment could not be added or resized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegError {
    /// It would overlap another segment or leave the user half.
    Overlap,
    /// There is no bss segment, or the address is below its start.
    BadAddress,
}

/// An address space and its segments: what a user process runs in.
#[derive(Debug)]
pub(crate) struct Memory {
    space: Space,
    /// Sorted by address; they never overlap.
    segs: SpinLock<Vec<Segment>>,
}

impl Memory {
    /// An empty address space.
    pub(crate) fn new() -> Option<Memory> {
        Some(Memory {
            space: Space::new()?,
            segs: SpinLock::new(Vec::new()),
        })
    }

    pub(crate) fn space(&self) -> &Space {
        &self.space
    }

    /// Adds a segment, which must not overlap another or reach past
    /// [`uaccess::USER_TOP`].
    pub(crate) fn add(&self, seg: Segment) -> Result<(), SegError> {
        if seg.top > uaccess::USER_TOP {
            return Err(SegError::Overlap);
        }
        let mut segs = self.segs.lock();
        if segs.iter().any(|s| s.base < seg.top && seg.base < s.top) {
            return Err(SegError::Overlap);
        }
        let at = segs.partition_point(|s| s.base < seg.base);
        segs.insert(at, seg);
        Ok(())
    }

    /// The end of the highest segment.
    pub(crate) fn segment_end(&self) -> u64 {
        self.segs.lock().last().map_or(0, |s| s.top)
    }

    /// The kind of the segment covering `va`.
    pub(crate) fn kind_at(&self, va: u64) -> Option<Kind> {
        self.segs.lock().iter().find(|s| s.contains(va)).map(|s| s.kind)
    }

    /// The frame for the page at `page` in `seg`, filled and mapped now if
    /// it was not yet.
    fn frame(&self, seg: &mut Segment, page: u64) -> Result<u64, FaultError> {
        if let Some(&f) = seg.pages.get(&page) {
            return Ok(f);
        }
        let frame = seg.fill(page).ok_or(FaultError::NoMemory)?;
        if self.space.map(page, frame, seg.kind.map_kind()).is_err() {
            // SAFETY: never mapped.
            unsafe { mm::free_frames(frame, 0) };
            return Err(FaultError::NoMemory);
        }
        seg.pages.insert(page, frame);
        Ok(frame)
    }

    /// Maps the page holding `va` for `access`, if a segment allows it.
    /// Runs with interrupts off, from the trap handler; mapping an empty
    /// place needs no TLB flush, so no shootdown waits here.
    pub(crate) fn fault(&self, va: u64, access: Access) -> Result<(), FaultError> {
        let mut segs = self.segs.lock();
        let seg = segs.iter_mut().find(|s| s.contains(va)).ok_or(FaultError::NoSegment)?;
        if !seg.kind.allows(access) {
            return Err(FaultError::Denied);
        }
        // Mapped already: another thread in the space got there first.
        self.frame(seg, va & !(PAGE - 1)).map(drop)
    }

    /// Copies `bytes` to `va` through the direct map, filling pages as
    /// needed, whatever the segments allow (for loading programs).
    pub(crate) fn write(&self, va: u64, bytes: &[u8]) -> Result<(), BadAddress> {
        self.each_page(va, bytes.len(), |seg, at, frame, done, n| {
            // SAFETY: `frame` is the segment's, in the direct map; the bytes
            // fit inside the page.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes[done..].as_ptr(), (phys_to_virt(frame) + at % PAGE) as *mut u8, n);
            }
            if seg == Kind::Text {
                arch::mmu::sync_icache(phys_to_virt(frame) + at % PAGE, n as u64);
            }
        })
    }

    /// Copies from `va` into `buf` through the direct map, filling pages
    /// as needed (for tests that look at a dead program's memory).
    pub(crate) fn read(&self, va: u64, buf: &mut [u8]) -> Result<(), BadAddress> {
        let len = buf.len();
        self.each_page(va, len, |_, at, frame, done, n| {
            // SAFETY: as for `write`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (phys_to_virt(frame) + at % PAGE) as *const u8,
                    buf[done..].as_mut_ptr(),
                    n,
                );
            }
        })
    }

    /// Calls `f(kind, va, frame, done, n)` for each page-sized piece of
    /// `[va, va + len)`: `n` bytes at `va`, `done` bytes in.
    fn each_page(
        &self,
        va: u64,
        len: usize,
        mut f: impl FnMut(Kind, u64, u64, usize, usize),
    ) -> Result<(), BadAddress> {
        uaccess::check(va, len)?;
        let mut segs = self.segs.lock();
        let mut done = 0;
        while done < len {
            let at = va + done as u64;
            #[allow(clippy::cast_possible_truncation)] // at most a page
            let n = ((PAGE - at % PAGE) as usize).min(len - done);
            let seg = segs.iter_mut().find(|s| s.contains(at)).ok_or(BadAddress)?;
            let frame = self.frame(seg, at & !(PAGE - 1)).map_err(|_| BadAddress)?;
            f(seg.kind, at, frame, done, n);
            done += n;
        }
        Ok(())
    }

    /// Moves the end of the bss segment to `addr`, rounded up to a page.
    /// Pages it gives up are unmapped, flushed and freed. Must be called
    /// with no spin lock held (`port/mmu.rs`).
    pub(crate) fn brk(&self, addr: u64) -> Result<(), SegError> {
        let top = addr.checked_next_multiple_of(PAGE).ok_or(SegError::Overlap)?;
        let gone: Vec<(u64, u64)> = {
            let mut segs = self.segs.lock();
            let i = segs
                .iter()
                .position(|s| s.kind == Kind::Bss)
                .ok_or(SegError::BadAddress)?;
            let next = segs.get(i + 1).map_or(uaccess::USER_TOP, |s| s.base);
            // The bss as linked may start inside the data's last page, so
            // an address there means the bss's own start, as in Plan 9.
            let base = segs[i].base;
            let in_data = i
                .checked_sub(1)
                .map(|d| &segs[d])
                .is_some_and(|d| d.kind == Kind::Data && d.top == base && addr >= d.base);
            let top = match addr >= base {
                true => top,
                false if in_data => base,
                false => return Err(SegError::BadAddress),
            };
            let bss = &mut segs[i];
            if top > next {
                return Err(SegError::Overlap);
            }
            let gone = bss.pages.split_off(&top).into_iter().collect();
            bss.top = top;
            gone
        };
        for (va, frame) in gone {
            // Another thread may have mapped it again only through a fault
            // in the segment, which no longer covers it.
            if self.space.unmap(va) == Some(frame) {
                // SAFETY: unmapped and flushed from every CPU in the space.
                unsafe { mm::free_frames(frame, 0) };
            }
        }
        Ok(())
    }
}

/// Resolves a fault on user address `addr` in the running process's
/// memory: true if the page is now mapped and the access should be retried.
/// Called by the architecture's trap handler, with interrupts off, for a
/// translation fault from user mode or from a user copy.
pub(crate) fn fault(addr: u64, access: Access) -> bool {
    if uaccess::check(addr, 1).is_err() {
        return false;
    }
    let Some(mem) = proc::current().and_then(|p| p.memory()) else {
        return false;
    };
    // The process holds its memory as well, so this is never the last
    // reference.
    mem.fault(addr, access).is_ok()
}
