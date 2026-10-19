//! Physical page frames: a binary buddy allocator.
//!
//! Memory is handed over as [`Frames::add`]ed ranges, each of which becomes
//! an *arena*. A block of order `k` is `2^k` pages, aligned to its own size
//! in physical address, so its buddy is the block whose address differs in
//! bit `12 + k` alone. Blocks never merge across arenas.
//!
//! Free blocks are kept on one doubly linked list per order, threaded
//! through the first bytes of each free block, which the allocator reaches
//! at `phys + offset` (the kernel's direct map). Each arena also keeps one
//! state byte per page, in pages taken from the arena's own start: for the
//! first page of a free block it holds [`FREE`] and the block's order, and
//! for every other page 0. That is what lets [`Frames::free`] find a free
//! buddy in constant time and catch a double free.
//!
//! The allocator has no lock of its own; the kernel wraps it in one.

#![cfg_attr(not(test), no_std)]

/// The page size.
pub const PAGE: u64 = 4096;
/// The number of block orders: the largest block is `2^(ORDERS-1)` pages
/// (1 GiB).
pub const ORDERS: usize = 19;
/// The most arenas there can be. Ranges beyond it are not used.
pub const MAX_ARENAS: usize = 128;

/// The state byte of a free block's first page: this flag and the order.
const FREE: u8 = 0x80;
/// A link that points nowhere.
const NIL: u64 = u64::MAX;

const _: () = assert!(ORDERS <= FREE as usize, "orders must fit beside the flag");

/// The state byte of a free block of `order`.
#[allow(clippy::cast_possible_truncation)] // less than ORDERS
const fn tag(order: usize) -> u8 {
    FREE | order as u8
}

/// `bytes` as a count of pages, for indexing.
#[allow(clippy::cast_possible_truncation)] // Plan R targets are 64-bit
const fn page_index(bytes: u64) -> usize {
    (bytes / PAGE) as usize
}

/// What [`Frames::free`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeError {
    /// The block is not in any arena, or not aligned to its order.
    NotOurs,
    /// The block, or its first page, is already free.
    AlreadyFree,
}

/// The links at the start of a free block.
#[repr(C)]
struct Link {
    next: u64,
    prev: u64,
}

#[derive(Debug, Clone, Copy)]
struct Arena {
    /// The first page the arena hands out.
    start: u64,
    /// One past its last page.
    end: u64,
    /// `(end - start) / PAGE` state bytes, at a virtual address.
    state: *mut u8,
}

impl Arena {
    const EMPTY: Arena = Arena {
        start: 0,
        end: 0,
        state: core::ptr::null_mut(),
    };

    fn contains(&self, phys: u64, bytes: u64) -> bool {
        self.start <= phys && phys.checked_add(bytes).is_some_and(|e| e <= self.end)
    }

    /// True if `phys` lies inside a larger free block.
    fn inside_free(&self, phys: u64) -> bool {
        (1..ORDERS).any(|k| {
            let head = phys & !((PAGE << k) - 1);
            // SAFETY: checked to be in the arena before it is read.
            self.contains(head, PAGE << k) && unsafe { *self.state(head) } == tag(k)
        })
    }

    fn state(&self, phys: u64) -> *mut u8 {
        // SAFETY (for callers that dereference it): `phys` is in the arena,
        // so the index is inside the state array.
        self.state.wrapping_add(page_index(phys - self.start))
    }
}

/// The allocator.
#[derive(Debug)]
pub struct Frames {
    /// `phys + offset` is where the allocator reaches `phys`.
    offset: u64,
    arenas: [Arena; MAX_ARENAS],
    count: usize,
    /// The first block on each order's free list, or [`NIL`].
    heads: [u64; ORDERS],
    free_pages: u64,
    total_pages: u64,
}

// SAFETY: the raw pointers point into memory the allocator owns; whoever
// shares a `Frames` between CPUs serializes access to it.
unsafe impl Send for Frames {}

impl Frames {
    /// An allocator with no memory, which reaches physical address `phys`
    /// at virtual address `phys + offset` (wrapping).
    #[must_use]
    pub const fn new(offset: u64) -> Frames {
        Frames {
            offset,
            arenas: [Arena::EMPTY; MAX_ARENAS],
            count: 0,
            heads: [NIL; ORDERS],
            free_pages: 0,
            total_pages: 0,
        }
    }

    /// Pages that can be allocated now.
    #[must_use]
    pub fn free_pages(&self) -> u64 {
        self.free_pages
    }

    /// Pages handed to the allocator, less those its state arrays use.
    #[must_use]
    pub fn total_pages(&self) -> u64 {
        self.total_pages
    }

    /// The number of arenas.
    #[must_use]
    pub fn arenas(&self) -> usize {
        self.count
    }

    /// Adds the RAM in `[start, end)`, rounded inwards to pages, and
    /// returns the number of pages that became free. Ranges must be added
    /// in increasing address order and must not overlap. A range too small
    /// to hold its own state and a page, or past [`MAX_ARENAS`], adds
    /// nothing.
    ///
    /// # Safety
    ///
    /// The range must be RAM that nothing else uses from now on, reachable
    /// for reading and writing at `phys + offset`.
    pub unsafe fn add(&mut self, start: u64, end: u64) -> u64 {
        let start = start.next_multiple_of(PAGE);
        let end = end & !(PAGE - 1);
        if end <= start || self.count == MAX_ARENAS {
            return 0;
        }
        if let Some(last) = self.count.checked_sub(1) {
            assert!(self.arenas[last].end <= start, "ranges added out of order");
        }
        let pages = (end - start) / PAGE;
        // Each state page covers PAGE pages, its own included.
        let meta = pages.div_ceil(PAGE + 1);
        if meta >= pages {
            return 0;
        }
        let arena = Arena {
            start: start + meta * PAGE,
            end,
            state: self.virt(start),
        };
        // SAFETY: the state array is in the caller's RAM, at its start.
        unsafe { core::ptr::write_bytes(arena.state, 0, page_index(end - arena.start)) };
        self.arenas[self.count] = arena;
        self.count += 1;
        let added = (end - arena.start) / PAGE;
        self.total_pages += added;
        // Cover the arena with the largest aligned blocks that fit.
        let mut at = arena.start;
        while at < end {
            let align = (at / PAGE).trailing_zeros() as usize;
            let fit = ((end - at) / PAGE).ilog2() as usize;
            let order = align.min(fit).min(ORDERS - 1);
            // SAFETY: the block is in the new arena and free.
            unsafe { self.push(&arena, at, order) };
            at += PAGE << order;
        }
        self.free_pages += added;
        added
    }

    /// Allocates a block of `2^order` pages, aligned to its size, and
    /// returns its physical address. Its contents are whatever they were.
    pub fn alloc(&mut self, order: usize) -> Option<u64> {
        let found = (order..ORDERS).find(|&k| self.heads[k] != NIL)?;
        let block = self.heads[found];
        let arena = *self.arena(block, PAGE << found)?;
        // SAFETY: the head of a free list is a free block of that order.
        unsafe { self.unlink(&arena, block, found) };
        // Split it, putting back the upper half each time.
        for k in (order..found).rev() {
            // SAFETY: the upper half is in the arena and now free.
            unsafe { self.push(&arena, block + (PAGE << k), k) };
        }
        self.free_pages -= 1 << order;
        Some(block)
    }

    /// Frees a block [`alloc`](Frames::alloc) returned, merging it with its
    /// free buddies.
    ///
    /// # Safety
    ///
    /// Nothing may use the block afterwards.
    pub unsafe fn free(&mut self, phys: u64, order: usize) -> Result<(), FreeError> {
        if order >= ORDERS || !phys.is_multiple_of(PAGE << order) {
            return Err(FreeError::NotOurs);
        }
        let arena = *self.arena(phys, PAGE << order).ok_or(FreeError::NotOurs)?;
        // A double free of a merged block shows only on its first page.
        // SAFETY: the page is in the arena.
        if unsafe { *arena.state(phys) } & FREE != 0 || arena.inside_free(phys) {
            return Err(FreeError::AlreadyFree);
        }
        let (mut block, mut k) = (phys, order);
        while k < ORDERS - 1 {
            let buddy = block ^ (PAGE << k);
            // SAFETY: checked to be in the arena before it is read.
            if !arena.contains(buddy, PAGE << k) || unsafe { *arena.state(buddy) } != tag(k) {
                break;
            }
            // SAFETY: the state says the buddy is a free block of order k.
            unsafe { self.unlink(&arena, buddy, k) };
            block = block.min(buddy);
            k += 1;
        }
        // SAFETY: the merged block is in the arena and free.
        unsafe { self.push(&arena, block, k) };
        self.free_pages += 1 << order;
        Ok(())
    }

    fn virt(&self, phys: u64) -> *mut u8 {
        #[allow(clippy::cast_possible_truncation)] // Plan R targets are 64-bit
        core::ptr::with_exposed_provenance_mut(phys.wrapping_add(self.offset) as usize)
    }

    fn link(&self, phys: u64) -> *mut Link {
        self.virt(phys).cast()
    }

    fn arena(&self, phys: u64, bytes: u64) -> Option<&Arena> {
        let arenas = &self.arenas[..self.count];
        let i = arenas.partition_point(|a| a.end <= phys);
        arenas.get(i).filter(|a| a.contains(phys, bytes))
    }

    /// Puts a free block on the front of its list.
    ///
    /// # Safety
    ///
    /// The block must be in `arena`, free, and on no list.
    unsafe fn push(&mut self, arena: &Arena, block: u64, order: usize) {
        let next = self.heads[order];
        // SAFETY: the block is free, so its first bytes are ours, and
        // `next`, if any, is a free block on this list.
        unsafe {
            self.link(block).write(Link { next, prev: NIL });
            if next != NIL {
                (*self.link(next)).prev = block;
            }
            *arena.state(block) = tag(order);
        }
        self.heads[order] = block;
    }

    /// Takes a free block off its list.
    ///
    /// # Safety
    ///
    /// The block must be in `arena` and on the list for `order`.
    unsafe fn unlink(&mut self, arena: &Arena, block: u64, order: usize) {
        // SAFETY: the block and its neighbours are free blocks on the list.
        unsafe {
            let Link { next, prev } = self.link(block).read();
            if prev == NIL {
                self.heads[order] = next;
            } else {
                (*self.link(prev)).next = next;
            }
            if next != NIL {
                (*self.link(next)).prev = prev;
            }
            *arena.state(block) = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pretend RAM: a page-aligned buffer standing in for physical memory
    /// at [`BASE`].
    struct Ram {
        mem: Vec<Page>,
    }

    #[repr(C, align(4096))]
    #[derive(Clone)]
    struct Page([u8; PAGE_BYTES]);

    const PAGE_BYTES: usize = 4096;

    /// Where the buffer pretends to be: aligned to the largest block.
    const BASE: u64 = 0x40_0000_0000;

    impl Ram {
        fn new(pages: usize) -> Ram {
            Ram {
                mem: vec![Page([0; PAGE_BYTES]); pages],
            }
        }

        fn frames(&mut self) -> Frames {
            let addr = self.mem.as_mut_ptr().expose_provenance() as u64;
            Frames::new(addr.wrapping_sub(BASE))
        }

        fn end(&self) -> u64 {
            BASE + self.mem.len() as u64 * PAGE
        }
    }

    fn add(f: &mut Frames, start: u64, end: u64) -> u64 {
        // SAFETY: the tests pass ranges inside their `Ram`.
        unsafe { f.add(start, end) }
    }

    fn free(f: &mut Frames, phys: u64, order: usize) -> Result<(), FreeError> {
        // SAFETY: the tests never use a block after freeing it.
        unsafe { f.free(phys, order) }
    }

    #[test]
    fn state_takes_one_page_per_4096() {
        let mut ram = Ram::new(4097 + 4);
        let mut f = ram.frames();
        assert_eq!(add(&mut f, BASE, BASE + 4097 * PAGE), 4096);
        assert_eq!(f.total_pages(), 4096);
        // 4098 pages need a second state page.
        let mut f = ram.frames();
        assert_eq!(add(&mut f, BASE, BASE + 4098 * PAGE), 4096);
        // Too small to hold anything.
        assert_eq!(add(&mut f, ram.end() - PAGE, ram.end()), 0);
    }

    #[test]
    fn blocks_are_aligned_and_distinct() {
        let mut ram = Ram::new(1024);
        let mut f = ram.frames();
        let n = add(&mut f, BASE, ram.end());
        let mut got = Vec::new();
        for order in [0, 3, 1, 5, 0, 2] {
            let b = f.alloc(order).unwrap();
            assert_eq!(b % (PAGE << order), 0, "order {order} block {b:#x} misaligned");
            got.push((b, b + (PAGE << order)));
        }
        got.sort_unstable();
        assert!(got.windows(2).all(|w| w[0].1 <= w[1].0), "blocks overlap: {got:x?}");
        assert!(got.iter().all(|&(s, e)| s >= BASE + PAGE && e <= ram.end()));
        assert_eq!(f.free_pages(), n - (1 + 8 + 2 + 32 + 1 + 4));
    }

    #[test]
    fn freeing_everything_merges_back() {
        let mut ram = Ram::new(1024);
        let mut f = ram.frames();
        // One state page, then 512 pages: one order-9 block.
        assert_eq!(add(&mut f, BASE + 511 * PAGE, ram.end()), 512);
        let all: Vec<u64> = (0..512).map(|_| f.alloc(0).unwrap()).collect();
        assert_eq!(f.alloc(0), None);
        for &p in all.iter().rev() {
            free(&mut f, p, 0).unwrap();
        }
        assert_eq!(f.free_pages(), 512);
        assert_eq!(f.alloc(9), Some(BASE + 512 * PAGE));
    }

    #[test]
    fn exhaustion_then_reuse() {
        let mut ram = Ram::new(64);
        let mut f = ram.frames();
        let n = add(&mut f, BASE, ram.end());
        assert_eq!(n, 63);
        assert_eq!(f.alloc(6), None, "no 64-page block in 63 pages");
        let big = f.alloc(5).unwrap();
        assert_eq!(f.alloc(5), None);
        free(&mut f, big, 5).unwrap();
        assert_eq!(f.alloc(5), Some(big));
    }

    #[test]
    fn double_and_foreign_frees_are_refused() {
        let mut ram = Ram::new(256);
        let mut f = ram.frames();
        add(&mut f, BASE, ram.end());
        let a = f.alloc(0).unwrap();
        let b = f.alloc(2).unwrap();
        free(&mut f, a, 0).unwrap();
        assert_eq!(free(&mut f, a, 0), Err(FreeError::AlreadyFree));
        free(&mut f, b, 2).unwrap();
        // b merged into a larger block: its pages are inside it.
        assert_eq!(free(&mut f, b + PAGE, 0), Err(FreeError::AlreadyFree));
        assert_eq!(free(&mut f, BASE, 0), Err(FreeError::NotOurs), "a state page");
        assert_eq!(free(&mut f, ram.end(), 0), Err(FreeError::NotOurs));
        assert_eq!(free(&mut f, b + PAGE, 2), Err(FreeError::NotOurs), "misaligned");
        assert_eq!(f.free_pages(), f.total_pages());
    }

    #[test]
    fn arenas_do_not_merge() {
        let mut ram = Ram::new(64);
        let mut f = ram.frames();
        // Two arenas of 31 pages each, back to back.
        let mid = BASE + 32 * PAGE;
        assert_eq!(add(&mut f, BASE, mid), 31);
        assert_eq!(add(&mut f, mid, ram.end()), 31);
        assert_eq!(f.arenas(), 2);
        let mut all = Vec::new();
        while let Some(p) = f.alloc(0) {
            all.push(p);
        }
        assert_eq!(all.len(), 62);
        for p in all {
            free(&mut f, p, 0).unwrap();
        }
        // 32 pages lie free on either side of mid's state page, but no
        // block spans it.
        assert_eq!(f.alloc(5), None);
        assert!(f.alloc(4).is_some());
    }

    #[test]
    fn random_workload_keeps_accounts() {
        let mut ram = Ram::new(2048);
        let mut f = ram.frames();
        let n = add(&mut f, BASE + 3 * PAGE, ram.end() - 5 * PAGE);
        let mut live: Vec<(u64, usize)> = Vec::new();
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if !seed.is_multiple_of(3) || live.is_empty() {
                let order = (seed >> 8) as usize % 6;
                if let Some(b) = f.alloc(order) {
                    live.push((b, order));
                }
            } else {
                let (b, order) = live.swap_remove((seed >> 16) as usize % live.len());
                free(&mut f, b, order).unwrap();
            }
            let used: u64 = live.iter().map(|&(_, o)| 1 << o).sum();
            assert_eq!(f.free_pages() + used, n);
        }
        let mut spans: Vec<(u64, u64)> = live.iter().map(|&(b, o)| (b, b + (PAGE << o))).collect();
        spans.sort_unstable();
        assert!(spans.windows(2).all(|w| w[0].1 <= w[1].0));
        for (b, o) in live {
            free(&mut f, b, o).unwrap();
        }
        assert_eq!(f.free_pages(), n);
    }
}
