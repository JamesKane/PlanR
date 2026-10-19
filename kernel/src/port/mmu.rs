//! Page tables: the kernel's own, address spaces for user mode, and TLB
//! shootdown.
//!
//! Both architectures use four levels of 512 entries over 48-bit
//! addresses, with 4 KiB pages and 2 MiB blocks; [`Mmu`] supplies each
//! one's descriptor encoding and translation registers, and [`Table`] walks
//! either.
//!
//! [`init`] builds the kernel's tables and leaves 9boot's: the direct map
//! of RAM, the framebuffer if it is not RAM, and the kernel image with
//! per-section permissions. Nothing is mapped in the lower half. The upper
//! half belongs to the kernel and is the same in every address space; the
//! lower half is a [`Space`]'s. A CPU with no process runs in
//! [`KERNEL_SPACE`], whose lower half is empty except for test pages.
//!
//! **TLB shootdown.** Changing or removing a valid mapping must reach every
//! CPU that may have cached it. arm64 broadcasts its invalidations in
//! hardware. amd64 sends the other CPUs an IPI and waits for each to flush.
//! Every CPU waiting for something here keeps answering other CPUs'
//! requests, so two shootdowns cannot deadlock each other. A CPU spinning
//! on a [`SpinLock`] has interrupts off, though, and cannot answer. So
//! **never call [`shootdown`] (or anything that unmaps) with a spin lock
//! held**: change the tables under the lock, drop it, then flush. A
//! shootdown that waits longer than [`SHOOTDOWN_TIMEOUT_NS`] panics and
//! says which CPU did not answer.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use bootinfo::{BootInfo, HHDM_BASE, phys_to_virt};

use crate::arch::{self, mmu::Native};
use crate::port::clock;
use crate::port::cpu::{self, MAXCPU};
use crate::port::lock::SpinLock;
use crate::port::mm::{self, MapKind, PAGE};

/// Bytes mapped by one block leaf at level 2.
pub(crate) const BLOCK: u64 = 2 << 20;
/// The level shifts, root first.
const SHIFTS: [u32; 4] = [39, 30, 21, 12];
/// The first root index of the upper (kernel) half.
const KERNEL_HALF: usize = 256;
/// The longest a shootdown waits for the other CPUs.
pub(crate) const SHOOTDOWN_TIMEOUT_NS: u64 = 1_000_000_000;
/// Flushing more pages than this flushes everything instead.
const FLUSH_ALL_PAGES: u64 = 32;

/// An architecture's page-table format and translation registers.
pub(crate) trait Mmu {
    /// True if one root covers both halves (amd64), so each address
    /// space's root carries the kernel half; false if the halves have
    /// roots of their own (arm64's `TTBR1`/`TTBR0`).
    const SHARED_ROOT: bool;
    /// True if [`flush_everywhere`](Mmu::flush_everywhere) reaches every
    /// CPU by itself, so no IPIs are needed.
    const BROADCAST_FLUSH: bool;

    /// A descriptor pointing at a next-level table. `user` is set for
    /// tables in the lower half.
    fn table(phys: u64, user: bool) -> u64;
    /// A leaf: a 4 KiB page, or a 2 MiB block if `block`.
    fn leaf(phys: u64, kind: MapKind, block: bool) -> u64;
    fn is_valid(e: u64) -> bool;
    /// True if `e`, found at the level with `shift`, points at a table.
    fn is_table(e: u64, shift: u32) -> bool;
    fn address(e: u64) -> u64;

    /// Makes entry stores visible to the table walker.
    fn publish();
    /// Switches to the kernel's tables for good.
    ///
    /// # Safety
    ///
    /// `kernel` maps the kernel image, the current stack and everything
    /// in use, at the addresses they are used at; `user` is a lower-half
    /// root (equal to `kernel` if [`SHARED_ROOT`](Mmu::SHARED_ROOT)).
    unsafe fn load(kernel: u64, user: u64);
    /// Makes `user` this CPU's lower half and drops what the TLB holds of
    /// the previous one.
    ///
    /// # Safety
    ///
    /// `user` is a live root; with a shared root, it carries the kernel
    /// half.
    unsafe fn activate(user: u64);
    /// Drops this CPU's translations of `pages` pages from `va`.
    fn flush_local(va: u64, pages: u64);
    /// Drops every CPU's translations of `pages` pages from `va`, if
    /// [`BROADCAST_FLUSH`](Mmu::BROADCAST_FLUSH); otherwise this CPU's.
    fn flush_everywhere(va: u64, pages: u64);
}

/// Why a mapping was not made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MapError {
    NoMemory,
    /// Something is mapped there already, or a block covers it.
    Mapped,
    /// A user mapping in the upper half or a kernel mapping in the lower.
    WrongHalf,
}

/// A page-table tree, by the physical address of its root.
#[derive(Debug)]
pub(crate) struct Table {
    root: u64,
}

fn entries(phys: u64) -> *mut u64 {
    phys_to_virt(phys) as *mut u64
}

fn index(va: u64, shift: u32) -> usize {
    ((va >> shift) & 511) as usize
}

fn is_user(va: u64) -> bool {
    va >> 63 == 0
}

impl Table {
    const fn empty() -> Table {
        Table { root: 0 }
    }

    /// A new tree with an empty root.
    fn new() -> Option<Table> {
        Some(Table {
            root: mm::alloc_frame()?,
        })
    }

    /// The entry for `va` at the level with `stop`, making the tables on
    /// the way if `create`.
    fn slot(&mut self, va: u64, stop: u32, create: bool) -> Result<*mut u64, MapError> {
        let mut t = self.root;
        for shift in SHIFTS {
            // SAFETY: `t` is a live table page, reached through the direct map.
            let slot = unsafe { entries(t).add(index(va, shift)) };
            if shift == stop {
                return Ok(slot);
            }
            // SAFETY: as above.
            let e = unsafe { *slot };
            if !Native::is_valid(e) {
                if !create {
                    return Err(MapError::Mapped);
                }
                let next = mm::alloc_frame().ok_or(MapError::NoMemory)?;
                // SAFETY: as above; the new table is zeroed, so nothing is
                // reachable through it yet.
                unsafe { *slot = Native::table(next, is_user(va)) };
                t = next;
            } else if Native::is_table(e, shift) {
                t = Native::address(e);
            } else {
                return Err(MapError::Mapped);
            }
        }
        unreachable!("the walk stops at a level")
    }

    /// Maps one page, or a block if `block`, at `va`. The place must be
    /// empty: replacing a valid entry would need a TLB flush.
    fn map(&mut self, va: u64, pa: u64, kind: MapKind, block: bool) -> Result<(), MapError> {
        if kind.is_user() != is_user(va) {
            return Err(MapError::WrongHalf);
        }
        self.put(va, pa, kind, block)
    }

    /// [`map`](Table::map) without the check that the kind suits the half.
    fn put(&mut self, va: u64, pa: u64, kind: MapKind, block: bool) -> Result<(), MapError> {
        let slot = self.slot(va, if block { 21 } else { 12 }, true)?;
        // SAFETY: `slot` is an entry in a live table.
        unsafe {
            if Native::is_valid(*slot) {
                return Err(MapError::Mapped);
            }
            *slot = Native::leaf(pa, kind, block);
        }
        Native::publish();
        Ok(())
    }

    /// Maps `[va, va + len)` to `[pa, pa + len)`, page aligned, with blocks
    /// where alignment allows.
    fn map_range(&mut self, mut va: u64, mut pa: u64, len: u64, kind: MapKind) -> Result<(), MapError> {
        let end = va + len;
        while va < end {
            let block = va.is_multiple_of(BLOCK) && pa.is_multiple_of(BLOCK) && end - va >= BLOCK;
            self.put(va, pa, kind, block)?;
            let step = if block { BLOCK } else { PAGE };
            va += step;
            pa += step;
        }
        Ok(())
    }

    /// Removes the page at `va` and returns what it mapped. The caller
    /// must flush the TLB ([`shootdown`]) before reusing the frame.
    fn unmap(&mut self, va: u64) -> Option<u64> {
        let slot = self.slot(va, 12, false).ok()?;
        // SAFETY: `slot` is an entry in a live table.
        let e = unsafe { slot.replace(0) };
        Native::publish();
        Native::is_valid(e).then(|| Native::address(e))
    }

    /// The physical address `va` maps to.
    pub(crate) fn translate(&mut self, va: u64) -> Option<u64> {
        let mut t = self.root;
        for shift in SHIFTS {
            // SAFETY: `t` is a live table page.
            let e = unsafe { *entries(t).add(index(va, shift)) };
            if !Native::is_valid(e) {
                return None;
            }
            if shift == 12 || !Native::is_table(e, shift) {
                let size = 1u64 << shift;
                return Some(Native::address(e) & !(size - 1) | (va & (size - 1)));
            }
            t = Native::address(e);
        }
        None
    }

    /// Frees the tables under the root's entries `[from, to)`, not the
    /// frames the leaves map, and clears those entries.
    fn free_tables(&mut self, from: usize, to: usize) {
        fn free_level(table: u64, shift: u32) {
            for i in 0..512 {
                // SAFETY: `table` is a live table page.
                let e = unsafe { *entries(table).add(i) };
                if Native::is_valid(e) && shift > 12 && Native::is_table(e, shift) {
                    free_level(Native::address(e), shift - 9);
                }
            }
            // SAFETY: the table is no longer reachable from any root.
            unsafe { mm::free_frames(table, 0) };
        }
        for i in from..to {
            // SAFETY: the root is a live table page.
            let slot = unsafe { entries(self.root).add(i) };
            // SAFETY: as above.
            let e = unsafe { slot.replace(0) };
            if Native::is_valid(e) {
                free_level(Native::address(e), 30);
            }
        }
    }
}

/// The kernel half's tables. On amd64 the root is also
/// [`KERNEL_SPACE`]'s; the two lock different halves of it.
static KERNEL: SpinLock<Table> = SpinLock::new(Table::empty());

/// The address space of a CPU with no process.
pub(crate) static KERNEL_SPACE: Space = Space::empty();

unsafe extern "C" {
    static __kernel_start: u8;
    static __text_end: u8;
    static __rodata_end: u8;
    static __kernel_end: u8;
}

/// Builds the kernel's tables and switches the boot CPU to them.
/// Called once, before anything is mapped outside the direct map.
pub(crate) fn init(info: &BootInfo) -> Result<(), MapError> {
    let mut k = Table::new().ok_or(MapError::NoMemory)?;
    if Native::SHARED_ROOT {
        // Every address space copies the kernel half's root entries, so
        // they must never change: make them all now.
        for i in KERNEL_HALF..512 {
            let t = mm::alloc_frame().ok_or(MapError::NoMemory)?;
            // SAFETY: the root is ours and not in use.
            unsafe { *entries(k.root).add(i) = Native::table(t, false) };
        }
    }
    // The direct map: RAM of any kind, merged across kinds as 9boot does,
    // so blocks can span them.
    let mut ram = info.memory().iter().filter(|r| r.kind.is_ram()).peekable();
    while let Some(r) = ram.next() {
        let (start, mut end) = (r.start, r.end());
        while let Some(next) = ram.next_if(|n| n.start == end) {
            end = next.end();
        }
        k.map_range(HHDM_BASE + start, start, end - start, MapKind::NormalReadWrite)?;
    }
    // The framebuffer, if it is not RAM, for the early progress marks.
    if let Some(fb) = info.framebuffer() {
        let (start, end) = (fb.phys & !(PAGE - 1), (fb.phys + fb.size).next_multiple_of(PAGE));
        if !info
            .memory()
            .iter()
            .any(|r| r.kind.is_ram() && r.start < end && start < r.end())
        {
            k.map_range(HHDM_BASE + start, start, end - start, MapKind::WriteCombining)?;
        }
    }
    // The kernel image, W^X.
    let sections = [
        (&raw const __kernel_start, &raw const __text_end, MapKind::KernelText),
        (&raw const __text_end, &raw const __rodata_end, MapKind::NormalReadOnly),
        (
            &raw const __rodata_end,
            &raw const __kernel_end,
            MapKind::NormalReadWrite,
        ),
    ];
    for (start, end, kind) in sections {
        let (start, end) = (start as u64, end as u64);
        for va in (0..(end - start) / PAGE).map(|i| start + i * PAGE) {
            k.map(va, mm::kernel_phys(va), kind, false)?;
        }
    }
    let user = if Native::SHARED_ROOT {
        k.root
    } else {
        mm::alloc_frame().ok_or(MapError::NoMemory)?
    };
    // SAFETY: the new tables map everything the old ones did that is in
    // use: the image, the direct map (which holds the boot stack and the
    // handoff), and the framebuffer. Nothing else has been mapped yet.
    unsafe { Native::load(k.root, user) };
    *KERNEL.lock() = k;
    KERNEL_SPACE.table.lock().root = user;
    KERNEL_SPACE.mark_active();
    Ok(())
}

/// The kernel half's tables, locked.
pub(crate) fn kernel() -> KernelTables {
    KernelTables(KERNEL.lock())
}

/// The kernel half's tables, held.
#[derive(Debug)]
pub(crate) struct KernelTables(crate::port::lock::SpinGuard<'static, Table>);

impl KernelTables {
    /// Maps one kernel page.
    pub(crate) fn map(&mut self, va: u64, pa: u64, kind: MapKind) -> Result<(), MapError> {
        self.0.map(va, pa, kind, false)
    }

    /// Unmaps one kernel page and returns its frame. Drop the tables, then
    /// [`shootdown`] the page, before reusing the frame.
    pub(crate) fn unmap(&mut self, va: u64) -> Option<u64> {
        self.0.unmap(va)
    }

    pub(crate) fn translate(&mut self, va: u64) -> Option<u64> {
        self.0.translate(va)
    }

    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))] // the amd64 AP trampoline
    pub(crate) fn root(&self) -> u64 {
        self.0.root
    }
}

/// A lower half: the user part of an address space.
#[derive(Debug)]
pub(crate) struct Space {
    table: SpinLock<Table>,
    /// The CPUs running in this space, by index.
    active: AtomicU64,
}

const _: () = assert!(MAXCPU <= 64, "Space::active is one word");

/// The space each CPU runs in, as a pointer for [`Space::activate`].
static CURRENT: [AtomicUsize; MAXCPU] = [const { AtomicUsize::new(0) }; MAXCPU];

impl Space {
    const fn empty() -> Space {
        Space {
            table: SpinLock::new(Table::empty()),
            active: AtomicU64::new(0),
        }
    }

    /// A new, empty address space.
    pub(crate) fn new() -> Option<Space> {
        let t = Table::new()?;
        if Native::SHARED_ROOT {
            let kernel = KERNEL.lock().root;
            // SAFETY: both roots are live table pages; the kernel half's
            // entries never change after `init`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    entries(kernel).add(KERNEL_HALF),
                    entries(t.root).add(KERNEL_HALF),
                    512 - KERNEL_HALF,
                );
            }
        }
        Some(Space {
            table: SpinLock::new(t),
            active: AtomicU64::new(0),
        })
    }

    /// Maps one user page.
    pub(crate) fn map(&self, va: u64, pa: u64, kind: MapKind) -> Result<(), MapError> {
        if !kind.is_user() {
            return Err(MapError::WrongHalf);
        }
        self.table.lock().map(va, pa, kind, false)
    }

    /// Unmaps one user page, flushes it from every CPU running in this
    /// space, and returns its frame, which is then free to reuse. Must be
    /// called with no spin lock held (see the module comment).
    pub(crate) fn unmap(&self, va: u64) -> Option<u64> {
        let pa = self.table.lock().unmap(va)?;
        shootdown(self.active.load(Ordering::Acquire), va, 1);
        Some(pa)
    }

    /// Makes this the calling CPU's address space.
    ///
    /// # Safety
    ///
    /// The space must stay alive until the CPU activates another.
    pub(crate) unsafe fn activate(&self) {
        let me = cpu::current().index();
        let root = self.table.lock().root;
        let this = core::ptr::from_ref(self).expose_provenance();
        let prev = CURRENT[me].swap(this, Ordering::AcqRel);
        if prev != 0 && prev != this {
            // SAFETY: the previous space was live until now, by the
            // contract of the `activate` that made it current.
            let prev = unsafe { &*core::ptr::with_exposed_provenance::<Space>(prev) };
            prev.active.fetch_and(!(1 << me), Ordering::AcqRel);
        }
        self.active.fetch_or(1 << me, Ordering::AcqRel);
        // SAFETY: the root is live; with a shared root it carries the
        // kernel half, copied in `new`.
        unsafe { Native::activate(root) };
    }

    /// Records that the calling CPU runs in this space, whose root it has
    /// already loaded (start-up).
    fn mark_active(&self) {
        let me = cpu::current().index();
        CURRENT[me].store(core::ptr::from_ref(self).expose_provenance(), Ordering::Release);
        self.active.fetch_or(1 << me, Ordering::AcqRel);
    }
}

impl Drop for Space {
    fn drop(&mut self) {
        assert!(
            *self.active.get_mut() == 0,
            "address space dropped while a CPU runs in it"
        );
        let t = self.table.get_mut();
        t.free_tables(0, KERNEL_HALF);
        // SAFETY: the root is ours and no CPU uses it.
        unsafe { mm::free_frames(t.root, 0) };
    }
}

/// Makes [`KERNEL_SPACE`] the calling CPU's address space. An application
/// processor calls this first, to leave its start-up tables.
pub(crate) fn activate_kernel_space() {
    // SAFETY: the kernel space is static.
    unsafe { KERNEL_SPACE.activate() };
}

/// Tables for starting the other CPUs: a lower half that maps the start-up
/// code at its physical address, so it keeps running while the MMU comes
/// on, and the kernel half. Dropped once every CPU has left it for
/// [`KERNEL_SPACE`] ([`leave_startup`]).
#[derive(Debug)]
pub(crate) struct StartupMap {
    table: Table,
    /// False if the root is the caller's.
    owns_root: bool,
}

impl StartupMap {
    /// A start-up map with a root of its own.
    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))] // arm64 AP start-up
    pub(crate) fn new() -> Option<StartupMap> {
        // SAFETY: a new root is a zeroed table page.
        unsafe { Some(StartupMap::new_in(mm::alloc_frame()?, true)) }
    }

    /// A start-up map in the page at `root` (amd64's trampoline needs its
    /// root below 4 GiB).
    ///
    /// # Safety
    ///
    /// `root` is a page of RAM in the direct map that nothing else uses
    /// while the map lives.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))] // the amd64 AP trampoline
    pub(crate) unsafe fn new_at(root: u64) -> StartupMap {
        // SAFETY: the caller's contract.
        unsafe { StartupMap::new_in(root, false) }
    }

    unsafe fn new_in(root: u64, owns_root: bool) -> StartupMap {
        // SAFETY: the caller's contract: the page is ours.
        unsafe {
            core::ptr::write_bytes(entries(root), 0, 512);
            if Native::SHARED_ROOT {
                let kernel = KERNEL.lock().root;
                core::ptr::copy_nonoverlapping(
                    entries(kernel).add(KERNEL_HALF),
                    entries(root).add(KERNEL_HALF),
                    512 - KERNEL_HALF,
                );
            }
        }
        StartupMap {
            table: Table { root },
            owns_root,
        }
    }

    /// Maps `[phys, phys + len)` at the same addresses, as kernel code.
    pub(crate) fn map(&mut self, phys: u64, len: u64) -> Result<(), MapError> {
        let start = phys & !(PAGE - 1);
        let end = (phys + len).next_multiple_of(PAGE);
        for pa in (0..(end - start) / PAGE).map(|i| start + i * PAGE) {
            self.table.put(pa, pa, MapKind::KernelText, false)?;
        }
        Ok(())
    }

    pub(crate) fn root(&self) -> u64 {
        self.table.root
    }
}

impl Drop for StartupMap {
    fn drop(&mut self) {
        self.table.free_tables(0, KERNEL_HALF);
        if self.owns_root {
            // SAFETY: no CPU uses the root any more.
            unsafe { mm::free_frames(self.table.root, 0) };
        }
    }
}

/// Moves a CPU that has just started from its [`StartupMap`] to
/// [`KERNEL_SPACE`], and drops every translation it holds, since the
/// start-up map's entries are global on arm64.
pub(crate) fn leave_startup() {
    activate_kernel_space();
    Native::flush_local(0, u64::MAX);
}

/// The one shootdown in flight: what to flush, and how many CPUs have yet
/// to.
static SHOOT_BUSY: AtomicBool = AtomicBool::new(false);
static SHOOT_VA: AtomicU64 = AtomicU64::new(0);
static SHOOT_PAGES: AtomicU64 = AtomicU64::new(0);
static SHOOT_LEFT: AtomicUsize = AtomicUsize::new(0);
/// Set for each CPU asked to flush, cleared as it does.
static SHOOT_ASKED: [AtomicBool; MAXCPU] = [const { AtomicBool::new(false) }; MAXCPU];

/// Flushes `pages` pages at `va` from this CPU and from every other CPU
/// in `cpus` (a mask of indices), and returns once they all have. Use
/// `u64::MAX` for every CPU (kernel mappings). Must be called with no spin
/// lock held.
pub(crate) fn shootdown(cpus: u64, va: u64, pages: u64) {
    if Native::BROADCAST_FLUSH {
        Native::flush_everywhere(va, pages);
        return;
    }
    Native::flush_local(va, pages);
    let me = cpu::current().index();
    let others: u64 = cpu::online()
        .map(cpu::Cpu::index)
        .filter(|&i| i != me && cpus & (1 << i) != 0)
        .fold(0, |m, i| m | 1 << i);
    if others == 0 {
        return;
    }
    while SHOOT_BUSY
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        answer_shootdown();
        core::hint::spin_loop();
    }
    SHOOT_VA.store(va, Ordering::Relaxed);
    SHOOT_PAGES.store(pages, Ordering::Relaxed);
    SHOOT_LEFT.store(others.count_ones() as usize, Ordering::Release);
    for i in (0..MAXCPU).filter(|i| others & (1 << i) != 0) {
        SHOOT_ASKED[i].store(true, Ordering::Release);
        if let Some(c) = cpu::get(i) {
            arch::send_ipi(c, arch::SHOOTDOWN_VECTOR);
        }
    }
    let done = clock::spin_until(
        || {
            answer_shootdown();
            SHOOT_LEFT.load(Ordering::Acquire) == 0
        },
        SHOOTDOWN_TIMEOUT_NS,
    );
    if !done {
        let late = (0..MAXCPU).find(|&i| SHOOT_ASKED[i].load(Ordering::Acquire));
        panic!("mmu: TLB shootdown: CPU {late:?} did not answer (spin lock held?)");
    }
    SHOOT_BUSY.store(false, Ordering::Release);
}

/// The shootdown IPI's handler, also polled by CPUs waiting in
/// [`shootdown`]: flushes what was asked of this CPU, if anything.
pub(crate) fn answer_shootdown() {
    let me = cpu::current().index();
    if SHOOT_ASKED[me].swap(false, Ordering::AcqRel) {
        Native::flush_local(SHOOT_VA.load(Ordering::Relaxed), SHOOT_PAGES.load(Ordering::Relaxed));
        SHOOT_LEFT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// True if flushing `pages` pages is better done by flushing everything.
pub(crate) fn flush_all_instead(pages: u64) -> bool {
    pages > FLUSH_ALL_PAGES
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::{Space, kernel, shootdown};
    use crate::arch;
    use crate::port::cpu::ktests::holds_everywhere;
    use crate::port::ktest::TestResult;
    use crate::port::mm::{self, MapKind, PAGE};

    /// The kernel's own tables map its text to where 9boot loaded it.
    pub(crate) fn image_translates() -> TestResult {
        let f = image_translates as fn() -> TestResult as *const () as u64;
        let mut k = kernel();
        let pa = k.translate(f).ok_or("kernel text not mapped")?;
        if pa != mm::kernel_phys(f) {
            return Err("kernel text maps the wrong frame".into());
        }
        Ok(())
    }

    /// A user page in one space is absent from another, and visible again
    /// once its space is active.
    pub(crate) fn spaces_are_separate() -> TestResult {
        const VA: u64 = 0x0000_0000_4000_0000;
        let a = Space::new().ok_or("no memory for a space")?;
        let frame = mm::alloc_frame().ok_or("no memory for a page")?;
        // SAFETY: the frame is ours and in the direct map.
        unsafe { (bootinfo::phys_to_virt(frame) as *mut u64).write(0x5bace) };
        a.map(VA, frame, MapKind::UserReadWrite).map_err(|_| "cannot map")?;
        // SAFETY: `a` outlives its activation: the kernel space returns below.
        unsafe { a.activate() };
        let seen = arch::probe_user_read(VA);
        super::activate_kernel_space();
        let gone = arch::probe_user_read(VA);
        let pa = a.unmap(VA);
        drop(a);
        // SAFETY: unmapped and flushed.
        unsafe { mm::free_frames(frame, 0) };
        if seen != Some(0x5bace) {
            return Err("page not visible in its space".into());
        }
        if gone.is_some() {
            return Err("page visible in the kernel space".into());
        }
        if pa != Some(frame) {
            return Err("unmap returned the wrong frame".into());
        }
        Ok(())
    }

    /// An unmapped kernel page faults on every CPU afterwards, though each
    /// had just read it (so had it in its TLB).
    pub(crate) fn shootdown_reaches_all() -> TestResult {
        static VA: AtomicU64 = AtomicU64::new(0);
        let va = mm::map_phys(mm::alloc_frame().ok_or("no memory")?, PAGE, MapKind::NormalReadWrite)
            .ok_or("cannot map a page")?;
        VA.store(va, Ordering::Release);
        if !holds_everywhere(|| arch::probe_read(VA.load(Ordering::Acquire)) == Some(0))? {
            return Err("page not readable everywhere".into());
        }
        let frame = kernel().unmap(va).ok_or("page not mapped")?;
        shootdown(u64::MAX, va, 1);
        // SAFETY: unmapped and flushed everywhere.
        unsafe { mm::free_frames(frame, 0) };
        if !holds_everywhere(|| arch::probe_read(VA.load(Ordering::Acquire)).is_none())? {
            return Err("a CPU still reads the unmapped page".into());
        }
        Ok(())
    }
}
