//! A process's memory: an address space ([`Space`]) and the segments that
//! say what belongs in it.
//!
//! **Segments** are Plan 9's: text, data, bss and stack, each a page-aligned
//! range of the user half that owns the frames mapped in it. A segment may
//! have an *image*, file bytes that fill its pages (the program's text and
//! data); the rest of every page is zero.
//!
//! **Demand paging.** Nothing is mapped until it is touched. A fault on an
//! unmapped page of a segment ([`fault`]) fills a new frame and maps it,
//! and the access is retried. The architecture's trap handler calls in for
//! faults from user mode, and for faults the kernel takes in its own user
//! copies (`port/uaccess.rs`) before it takes their fixup, so a system call
//! on a buffer not yet touched works.
//!
//! **Sharing.** A segment may belong to several memories, as `rfork`
//! leaves them: text and attached shared segments always, data, bss and
//! attached memory with `RFMEM`. Each memory's
//! space maps the segment's pages as they are touched there, and the
//! segment keeps a list of those spaces, so that a page it gives up or
//! replaces is cleared from all of them. The stack is never shared.
//!
//! **Copy-on-write.** A page is a counted reference to its frame. `rfork`
//! without `RFMEM` gives the child a copy of the parent's data, bss and
//! stack segments that holds the same frames, and marks every page of
//! both copies copy-on-write: mapped read-only. A write fault on such a
//! page copies the frame if anything else still holds it, or else just
//! takes it back; either way the page is then the writer's alone.
//!
//! **Attaching.** `segattach` adds a segment of zeros, of class `shared`
//! (shared by every child) or `memory` (copied as data is), anywhere free
//! in the user half; `segdetach` takes one away again. Both survive
//! `exec` unless attached with `CEXEC`.
//!
//! **Freeing.** A frame goes back when the last page holding it lets go:
//! after the page was cleared from every space and flushed ([`Pending`]),
//! or with the last memory holding its segment, once no CPU runs in that
//! memory's space. Switching spaces drops the old one's translations on
//! both architectures, so no TLB still holds them then.
//!
//! **Locks.** A memory's segment list, then a segment's pages, then a
//! space's tables. A change that takes a translation away is flushed only
//! once all three are dropped (`port/mmu.rs`).

use alloc::collections::BTreeMap;
use alloc::collections::btree_map::Entry;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::AtomicU64;

use abi::SegAttr;
use bootinfo::phys_to_virt;

use crate::arch;
use crate::port::lock::SpinLock;
use crate::port::mm::{self, MapKind, PAGE};

const PAGE_BYTES: usize = 4096;
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
    /// Attached (`segattach`), shared by every child.
    Shared,
    /// Attached, copied for a child as data is.
    Memory,
}

impl Kind {
    /// The name `#p/n/segment` gives it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::Text => "Text",
            Kind::Data => "Data",
            Kind::Bss => "Bss",
            Kind::Stack => "Stack",
            Kind::Shared => "Shared",
            Kind::Memory => "Memory",
        }
    }

    fn attached(self) -> bool {
        matches!(self, Kind::Shared | Kind::Memory)
    }
}

/// A segment as `#p/n/segment` lists it.
#[derive(Debug, Clone)]
pub(crate) struct SegInfo {
    pub(crate) kind: Kind,
    pub(crate) ronly: bool,
    pub(crate) cexec: bool,
    pub(crate) base: u64,
    pub(crate) top: u64,
    /// The memories holding it.
    pub(crate) refs: usize,
}

/// What a faulting access tried to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
    Exec,
}

/// Bytes that fill a segment's pages: a file's contents where they lie
/// for good (the boot archive's, [`Image::mapped`]), or read into memory
/// shared by the segments that use them.
#[derive(Debug, Clone)]
pub(crate) struct Image {
    src: Source,
    start: usize,
    len: usize,
}

#[derive(Debug, Clone)]
enum Source {
    Static(&'static [u8]),
    Owned(Arc<[u8]>),
}

impl Image {
    /// No bytes: every page zero.
    pub(crate) const NONE: Image = Image {
        src: Source::Static(&[]),
        start: 0,
        len: 0,
    };

    pub(crate) fn mapped(b: &'static [u8]) -> Image {
        Image {
            src: Source::Static(b),
            start: 0,
            len: b.len(),
        }
    }

    pub(crate) fn owned(b: Arc<[u8]>) -> Image {
        let len = b.len();
        Image {
            src: Source::Owned(b),
            start: 0,
            len,
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        let all: &[u8] = match &self.src {
            Source::Static(b) => b,
            Source::Owned(b) => b,
        };
        &all[self.start..self.start + self.len]
    }

    /// `len` bytes from `start`, which must lie inside.
    pub(crate) fn slice(&self, start: usize, len: usize) -> Image {
        assert!(
            start.checked_add(len).is_some_and(|e| e <= self.len),
            "image slice out of range"
        );
        Image {
            src: self.src.clone(),
            start: self.start + start,
            len,
        }
    }
}

/// A frame holding one page of a segment, freed when the last page that
/// holds it is dropped.
#[derive(Debug)]
struct Frame(u64);

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: no page holds the frame any more, and each that did was
        // cleared from every space and flushed first, or went with the
        // last memory mapping it, once no CPU ran there (the module
        // comment).
        unsafe { mm::free_frames(self.0, 0) };
    }
}

/// A segment's page.
#[derive(Debug, Clone)]
struct Page {
    frame: Arc<Frame>,
    /// Copy-on-write: mapped read-only, and copied (or taken back) on
    /// the first write.
    cow: bool,
}

/// A range of the user half and its pages, which one memory or several
/// map.
#[derive(Debug)]
pub(crate) struct Segment {
    kind: Kind,
    /// Never written (an attached segment's `RONLY`); text never is.
    ronly: bool,
    /// Detached by `exec`.
    cexec: bool,
    base: u64,
    /// Bytes that belong at `image_at`, filling the pages they overlap.
    image: Image,
    image_at: u64,
    inner: SpinLock<SegInner>,
}

#[derive(Debug)]
struct SegInner {
    top: u64,
    /// The pages made so far, by address.
    pages: BTreeMap<u64, Page>,
    /// The spaces of the memories the segment belongs to; a page of it
    /// may be mapped in any of them.
    spaces: Vec<Weak<Space>>,
}

impl SegInner {
    /// The spaces still alive.
    fn live(&self) -> impl Iterator<Item = Arc<Space>> + '_ {
        self.spaces.iter().filter_map(Weak::upgrade)
    }

    /// Clears the page at `va` from every space, noting in `pending` the
    /// spaces to flush.
    fn clear(&self, va: u64, pending: &mut Pending) {
        for s in self.live() {
            if s.clear(va).is_some() {
                pending.flush(s, va, 1);
            }
        }
    }
}

/// What is left to do once the locks are dropped: flush the spaces that
/// lost translations, then free the frames they held.
#[derive(Debug, Default)]
struct Pending {
    flush: Vec<(Arc<Space>, u64, u64)>,
    free: Vec<Arc<Frame>>,
}

impl Pending {
    fn flush(&mut self, s: Arc<Space>, va: u64, pages: u64) {
        self.flush.push((s, va, pages));
    }

    /// Must be called with no spin lock held.
    fn finish(self) {
        for (s, va, pages) in &self.flush {
            s.flush(*va, *pages);
        }
        drop(self.free);
    }
}

impl Segment {
    /// A segment over `[base, top)`, both page aligned, filled from
    /// `image` at `image_at`.
    pub(crate) fn new(kind: Kind, base: u64, top: u64, image: Image, image_at: u64) -> Segment {
        assert!(
            base.is_multiple_of(PAGE) && top.is_multiple_of(PAGE) && base <= top,
            "segment not page aligned"
        );
        Segment {
            kind,
            ronly: false,
            cexec: false,
            base,
            image,
            image_at,
            inner: SpinLock::new(SegInner {
                top,
                pages: BTreeMap::new(),
                spaces: Vec::new(),
            }),
        }
    }

    /// How a page of the segment is mapped, read-only if `cow`.
    fn map_kind(&self, cow: bool) -> MapKind {
        match self.kind {
            Kind::Text => MapKind::UserText,
            _ if cow || self.ronly => MapKind::UserReadOnly,
            _ => MapKind::UserReadWrite,
        }
    }

    fn allows(&self, access: Access) -> bool {
        match access {
            Access::Read => true,
            Access::Write => self.kind != Kind::Text && !self.ronly,
            Access::Exec => self.kind == Kind::Text,
        }
    }

    fn top(&self) -> u64 {
        self.inner.lock().top
    }

    /// A new frame holding the page at `page`: the image where it overlaps,
    /// zeros elsewhere.
    fn fill(&self, page: u64) -> Option<u64> {
        let frame = mm::alloc_frame()?;
        let image = self.image.bytes();
        let (start, end) = (
            page.max(self.image_at),
            (page + PAGE).min(self.image_at + image.len() as u64),
        );
        if start < end {
            #[allow(clippy::cast_possible_truncation)] // inside the image
            let from = &image[(start - self.image_at) as usize..(end - self.image_at) as usize];
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

    /// Makes the page at `page` ready for `access` through `space` and
    /// returns its frame: made if new, copied (or taken back) if written
    /// while copy-on-write, and mapped in `space` as it may now be used.
    fn resolve(
        &self,
        inner: &mut SegInner,
        space: &Space,
        page: u64,
        access: Access,
        pending: &mut Pending,
    ) -> Result<u64, FaultError> {
        if !(self.base <= page && page < inner.top) {
            return Err(FaultError::NoSegment);
        }
        if !self.allows(access) {
            return Err(FaultError::Denied);
        }
        let p = match inner.pages.entry(page) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(Page {
                frame: Arc::new(Frame(self.fill(page).ok_or(FaultError::NoMemory)?)),
                cow: false,
            }),
        };
        let mut moved = false;
        if p.cow && access == Access::Write {
            // Anything else holding the frame would see the write, so the
            // page gets a copy and every space loses the old frame. Only
            // this segment can add a holder, and it is locked.
            if Arc::strong_count(&p.frame) > 1 {
                let copy = mm::alloc_frame().ok_or(FaultError::NoMemory)?;
                // SAFETY: both frames are whole pages in the direct map;
                // the copy is new and ours.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        phys_to_virt(p.frame.0) as *const u8,
                        phys_to_virt(copy) as *mut u8,
                        PAGE_BYTES,
                    );
                }
                pending
                    .free
                    .push(core::mem::replace(&mut p.frame, Arc::new(Frame(copy))));
                moved = true;
            }
            p.cow = false;
        }
        let (frame, kind) = (p.frame.0, self.map_kind(p.cow));
        if moved {
            inner.clear(page, pending);
        }
        match space.translate(page) {
            None => space.map(page, frame, kind).map_err(|_| FaultError::NoMemory)?,
            // Mapped already: read-only until now, or another thread got
            // there first, or this CPU still had an entry another changed.
            Some(f) if f == frame => {
                space.protect(page, kind);
                Space::flush_here(page);
            }
            Some(_) => unreachable!("a space maps a frame its segment does not hold"),
        }
        Ok(frame)
    }

    /// A copy for a child, holding the same frames, with every writable
    /// page made copy-on-write in both; the spaces that must lose their
    /// writable entries are noted in `pending`.
    fn copy(&self, pending: &mut Pending) -> Segment {
        let mut inner = self.inner.lock();
        let spaces: Vec<Arc<Space>> = inner.live().collect();
        let mut protected = false;
        for (&va, p) in &mut inner.pages {
            if self.kind != Kind::Text && !p.cow {
                p.cow = true;
                protected = true;
                for s in &spaces {
                    s.protect(va, MapKind::UserReadOnly);
                }
            }
        }
        if protected {
            for s in spaces {
                pending.flush(s, 0, u64::MAX);
            }
        }
        Segment {
            kind: self.kind,
            ronly: self.ronly,
            cexec: self.cexec,
            base: self.base,
            image: self.image.clone(),
            image_at: self.image_at,
            inner: SpinLock::new(SegInner {
                top: inner.top,
                pages: inner.pages.clone(),
                spaces: Vec::new(),
            }),
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
    space: Arc<Space>,
    /// Sorted by address; they never overlap.
    segs: SpinLock<Vec<Arc<Segment>>>,
}

impl Memory {
    /// An empty address space.
    pub(crate) fn new() -> Option<Memory> {
        Some(Memory {
            space: Arc::new(Space::new()?),
            segs: SpinLock::new(Vec::new()),
        })
    }

    pub(crate) fn space(&self) -> &Space {
        &self.space
    }

    /// Adds a segment, which must not overlap another or reach past
    /// [`uaccess::USER_TOP`].
    pub(crate) fn add(&self, seg: Segment) -> Result<(), SegError> {
        if seg.top() > uaccess::USER_TOP {
            return Err(SegError::Overlap);
        }
        let mut segs = self.segs.lock();
        if segs.iter().any(|s| s.base < seg.top() && seg.base < s.top()) {
            return Err(SegError::Overlap);
        }
        let at = segs.partition_point(|s| s.base < seg.base);
        let seg = Arc::new(seg);
        self.join(&seg);
        segs.insert(at, seg);
        Ok(())
    }

    /// Records that this memory's space may map `seg`'s pages.
    fn join(&self, seg: &Segment) {
        let mut inner = seg.inner.lock();
        inner.spaces.retain(|w| w.strong_count() > 0);
        inner.spaces.push(Arc::downgrade(&self.space));
    }

    /// A child's memory, as `rfork` makes it: the text shared, the data
    /// and bss shared too if `share`, and the rest copied on write. Must
    /// be called with no spin lock held, as the parent's writable pages
    /// become read-only and are flushed.
    pub(crate) fn fork(&self, share: bool) -> Option<Memory> {
        let child = Memory::new()?;
        let mut pending = Pending::default();
        {
            let segs = self.segs.lock();
            let mut copies = Vec::with_capacity(segs.len());
            for s in segs.iter() {
                let seg = match s.kind {
                    Kind::Text | Kind::Shared => s.clone(),
                    Kind::Data | Kind::Bss | Kind::Memory if share => s.clone(),
                    Kind::Data | Kind::Bss | Kind::Memory | Kind::Stack => Arc::new(s.copy(&mut pending)),
                };
                child.join(&seg);
                copies.push(seg);
            }
            *child.segs.lock() = copies;
        }
        pending.finish();
        Some(child)
    }

    /// The end of the highest segment.
    pub(crate) fn segment_end(&self) -> u64 {
        self.segs.lock().last().map_or(0, |s| s.top())
    }

    /// The segment covering `va`.
    fn segment_at(&self, va: u64) -> Option<Arc<Segment>> {
        self.segs.lock().iter().find(|s| s.base <= va && va < s.top()).cloned()
    }

    /// The kind of the segment covering `va`.
    pub(crate) fn kind_at(&self, va: u64) -> Option<Kind> {
        self.segment_at(va).map(|s| s.kind)
    }

    /// The frame for the page holding `va`, ready for `access` and mapped.
    /// Must be called with no spin lock held.
    fn page(&self, va: u64, access: Access) -> Result<u64, FaultError> {
        let seg = self.segment_at(va).ok_or(FaultError::NoSegment)?;
        let mut pending = Pending::default();
        let r = seg.resolve(
            &mut seg.inner.lock(),
            &self.space,
            va & !(PAGE - 1),
            access,
            &mut pending,
        );
        pending.finish();
        r
    }

    /// Maps the page holding `va` for `access`, if a segment allows it,
    /// copying it first if it is written while copy-on-write. Runs with
    /// interrupts off, from the trap handler, holding no spin lock.
    pub(crate) fn fault(&self, va: u64, access: Access) -> Result<(), FaultError> {
        self.page(va, access).map(drop)
    }

    /// Copies `bytes` to `va` through the direct map, filling pages as
    /// needed, whatever the segments allow (for loading programs).
    pub(crate) fn write(&self, va: u64, bytes: &[u8]) -> Result<(), BadAddress> {
        self.each_page(va, bytes.len(), true, |kind, at, frame, done, n| {
            // SAFETY: `frame` is the segment's, in the direct map, and not
            // shared (it was resolved for writing); the bytes fit inside
            // the page.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes[done..].as_ptr(), (phys_to_virt(frame) + at % PAGE) as *mut u8, n);
            }
            if kind == Kind::Text {
                arch::mmu::sync_icache(phys_to_virt(frame) + at % PAGE, n as u64);
            }
        })
    }

    /// Copies from `va` into `buf` through the direct map, filling pages
    /// as needed (for tests that look at a dead program's memory).
    pub(crate) fn read(&self, va: u64, buf: &mut [u8]) -> Result<(), BadAddress> {
        let len = buf.len();
        self.each_page(va, len, false, |_, at, frame, done, n| {
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
    /// `[va, va + len)`: `n` bytes at `va`, `done` bytes in. A page to be
    /// written is made the segment's own first.
    fn each_page(
        &self,
        va: u64,
        len: usize,
        write: bool,
        mut f: impl FnMut(Kind, u64, u64, usize, usize),
    ) -> Result<(), BadAddress> {
        uaccess::check(va, len)?;
        let mut done = 0;
        while done < len {
            let at = va + done as u64;
            #[allow(clippy::cast_possible_truncation)] // at most a page
            let n = ((PAGE - at % PAGE) as usize).min(len - done);
            let seg = self.segment_at(at).ok_or(BadAddress)?;
            let mut pending = Pending::default();
            // Text allows no writes, which loading it needs: ask for what
            // the segment allows, but still take a copy-on-write page.
            let access = match (write, seg.kind) {
                (true, Kind::Text) => Access::Exec,
                (true, _) => Access::Write,
                (false, _) => Access::Read,
            };
            let frame = seg.resolve(
                &mut seg.inner.lock(),
                &self.space,
                at & !(PAGE - 1),
                access,
                &mut pending,
            );
            pending.finish();
            f(seg.kind, at, frame.map_err(|_| BadAddress)?, done, n);
            done += n;
        }
        Ok(())
    }

    /// Moves the end of the bss segment to `addr`, rounded up to a page.
    /// Pages it gives up are cleared from every space that maps them,
    /// flushed and let go. Must be called with no spin lock held.
    pub(crate) fn brk(&self, addr: u64) -> Result<(), SegError> {
        let top = addr.checked_next_multiple_of(PAGE).ok_or(SegError::Overlap)?;
        let mut pending = Pending::default();
        {
            let segs = self.segs.lock();
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
                .is_some_and(|d| d.kind == Kind::Data && d.top() == base && addr >= d.base);
            let top = match addr >= base {
                true => top,
                false if in_data => base,
                false => return Err(SegError::BadAddress),
            };
            if top > next {
                return Err(SegError::Overlap);
            }
            let mut bss = segs[i].inner.lock();
            let gone = bss.pages.split_off(&top);
            if let Some(&last) = gone.keys().next_back() {
                for s in bss.live() {
                    for &va in gone.keys() {
                        s.clear(va);
                    }
                    pending.flush(s, top, (last + PAGE - top) / PAGE);
                }
            }
            bss.top = top;
            pending.free.extend(gone.into_values().map(|p| p.frame));
        }
        pending.finish();
        Ok(())
    }

    /// `segattach`: adds a segment of `kind` (attached: shared or memory)
    /// of `len` bytes rounded up to pages, at `va` rounded down to one, or,
    /// if `va` is 0, at the highest free place below the stack; returns
    /// its base.
    pub(crate) fn attach(&self, kind: Kind, attr: SegAttr, va: u64, len: u64) -> Result<u64, SegError> {
        assert!(kind.attached(), "only shared and memory segments attach");
        let len = len
            .checked_next_multiple_of(PAGE)
            .filter(|&l| l > 0)
            .ok_or(SegError::BadAddress)?;
        let mut segs = self.segs.lock();
        let base = if va == 0 {
            // Down from the stack, below each segment in the way.
            let mut top = segs
                .iter()
                .find(|s| s.kind == Kind::Stack)
                .map_or(uaccess::USER_TOP, |s| s.base);
            loop {
                let base = top.checked_sub(len).filter(|&b| b >= LOWEST).ok_or(SegError::Overlap)?;
                match segs.iter().rev().find(|s| s.base < top && base < s.top()) {
                    Some(s) => top = s.base,
                    None => break base,
                }
            }
        } else {
            va & !(PAGE - 1)
        };
        let top = base.checked_add(len).ok_or(SegError::Overlap)?;
        if base < LOWEST || top > uaccess::USER_TOP || segs.iter().any(|s| s.base < top && base < s.top()) {
            return Err(SegError::Overlap);
        }
        let mut seg = Segment::new(kind, base, top, Image::NONE, 0);
        seg.ronly = attr.contains(SegAttr::RONLY);
        seg.cexec = attr.contains(SegAttr::CEXEC);
        let seg = Arc::new(seg);
        self.join(&seg);
        let at = segs.partition_point(|s| s.base < base);
        segs.insert(at, seg);
        Ok(base)
    }

    /// `segdetach`: takes away the attached segment holding `va`; its pages
    /// leave this memory's space, and go once no other memory holds it.
    /// Must be called with no spin lock held.
    pub(crate) fn detach(&self, va: u64) -> Result<(), SegError> {
        let mut pending = Pending::default();
        let gone = {
            let mut segs = self.segs.lock();
            let i = segs
                .iter()
                .position(|s| s.kind.attached() && s.base <= va && va < s.top())
                .ok_or(SegError::BadAddress)?;
            let seg = segs.remove(i);
            let mut inner = seg.inner.lock();
            let me = Arc::as_ptr(&self.space);
            inner.spaces.retain(|w| w.as_ptr() != me);
            let mut cleared = false;
            for &va in inner.pages.keys() {
                cleared |= self.space.clear(va).is_some();
            }
            if cleared {
                pending.flush(self.space.clone(), seg.base, (inner.top - seg.base) / PAGE);
            }
            drop(inner);
            seg
        };
        pending.finish();
        drop(gone);
        Ok(())
    }

    /// `segfree`: lets go of the pages wholly inside `[va, va + len)`,
    /// which read as zeros (or their image) when next touched. Must be
    /// called with no spin lock held.
    pub(crate) fn free(&self, va: u64, len: u64) -> Result<(), SegError> {
        let from = va.checked_next_multiple_of(PAGE).ok_or(SegError::BadAddress)?;
        let to = va.checked_add(len).ok_or(SegError::BadAddress)? & !(PAGE - 1);
        let mut pending = Pending::default();
        {
            let segs = self.segs.lock();
            for s in segs.iter().filter(|s| s.base < to && from < s.top()) {
                let mut inner = s.inner.lock();
                let gone: Vec<u64> = inner.pages.range(from..to).map(|(&va, _)| va).collect();
                for va in gone {
                    inner.clear(va, &mut pending);
                    if let Some(p) = inner.pages.remove(&va) {
                        pending.free.push(p.frame);
                    }
                }
            }
        }
        pending.finish();
        Ok(())
    }

    /// True if segments cover all of `[va, va + len)` (`segflush`).
    pub(crate) fn covers(&self, va: u64, len: u64) -> bool {
        let Some(end) = va.checked_add(len) else {
            return false;
        };
        let segs = self.segs.lock();
        let mut at = va;
        while at < end {
            match segs.iter().find(|s| s.base <= at && at < s.top()) {
                Some(s) => at = s.top(),
                None => return false,
            }
        }
        true
    }

    /// Takes over `old`'s attached segments that `exec` keeps: those not
    /// attached with `CEXEC`.
    pub(crate) fn inherit(&self, old: &Memory) -> Result<(), SegError> {
        let kept: Vec<Arc<Segment>> = old
            .segs
            .lock()
            .iter()
            .filter(|s| s.kind.attached() && !s.cexec)
            .cloned()
            .collect();
        let mut segs = self.segs.lock();
        for seg in kept {
            if segs.iter().any(|s| s.base < seg.top() && seg.base < s.top()) {
                return Err(SegError::Overlap);
            }
            self.join(&seg);
            let at = segs.partition_point(|s| s.base < seg.base);
            segs.insert(at, seg);
        }
        Ok(())
    }

    /// The segments, for `#p/n/segment`.
    pub(crate) fn info(&self) -> Vec<SegInfo> {
        self.segs
            .lock()
            .iter()
            .map(|s| SegInfo {
                kind: s.kind,
                ronly: s.ronly,
                cexec: s.cexec,
                base: s.base,
                top: s.top(),
                refs: Arc::strong_count(s) - 1,
            })
            .collect()
    }

    /// Runs `f` on the word at `va`, 8-byte aligned, made writable first,
    /// under its segment's lock; returns what `f` did and the segment's
    /// identity, which names the word with `va` across the memories that
    /// share it. Must be called with no spin lock held.
    pub(crate) fn with_word<R>(&self, va: u64, f: impl FnOnce(&AtomicU64) -> R) -> Result<(R, usize), BadAddress> {
        uaccess::check(va, 8)?;
        if !va.is_multiple_of(8) {
            return Err(BadAddress);
        }
        let seg = self.segment_at(va).ok_or(BadAddress)?;
        let mut pending = Pending::default();
        let r = {
            let mut inner = seg.inner.lock();
            seg.resolve(&mut inner, &self.space, va & !(PAGE - 1), Access::Write, &mut pending)
                .map(|frame| {
                    // SAFETY: the frame is the segment's, written only as
                    // whole aligned words through `AtomicU64`s here and by
                    // user code, and it stays while the segment is locked.
                    f(unsafe { &*((phys_to_virt(frame) + va % PAGE) as *const AtomicU64) })
                })
        };
        pending.finish();
        Ok((r.map_err(|_| BadAddress)?, Arc::as_ptr(&seg) as usize))
    }

    /// Runs `f` on the word at `va` if its page is there and the
    /// segment's own (as [`with_word`](Memory::with_word) leaves it),
    /// changing nothing else, so it may run under a spin lock.
    pub(crate) fn peek_word<R>(&self, va: u64, f: impl FnOnce(&AtomicU64) -> R) -> Option<R> {
        let seg = self.segment_at(va)?;
        let inner = seg.inner.lock();
        let p = inner.pages.get(&(va & !(PAGE - 1))).filter(|p| !p.cow)?;
        // SAFETY: as for `with_word`.
        Some(f(unsafe {
            &*((phys_to_virt(p.frame.0) + va % PAGE) as *const AtomicU64)
        }))
    }
}

/// Nothing is attached below this, so null pointers and small offsets
/// from them always fault (as `exec` leaves the bottom of the user half).
const LOWEST: u64 = 0x1_0000;

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
