//! The heap: `lib/heap`'s size classes, the kernel's own, whose slabs and
//! larger blocks come from [`Blocks`], page blocks carved from the end of
//! the bss as `brk` moves it.
//!
//! Blocks are powers of two in pages, aligned to their size. A freed block
//! goes on its size's free list and is never given back to the system or
//! merged with its neighbour; a request with no block of its size free
//! splits a larger free one, else takes the next aligned block at the
//! break. The aligned gap below that block is not wasted: it is cut into
//! the largest aligned blocks that fit, which go on their free lists.
//!
//! [`Arena`] is the allocator itself, with no lock; the program's
//! `#[global_allocator]` holds one around it, since processes made with
//! `RFMEM` share the heap.

use core::alloc::Layout;
use core::ptr::{self, NonNull};

use heap::{CLASSES, Class, PAGE, Pages, Size};

/// The largest block: `2^(ORDERS - 1)` pages.
const ORDERS: usize = 32;

/// Where the break is, and moving it.
pub trait Break {
    /// The break as the program started.
    fn initial(&self) -> usize;
    /// Moves the break up to `to`; false if it cannot.
    fn grow(&mut self, to: usize) -> bool;
}

/// Page blocks from the break.
#[derive(Debug)]
pub struct Blocks<B> {
    brk: B,
    /// Where the next block is carved; 0 until the first.
    top: usize,
    /// Free blocks by order, each holding a pointer to the next.
    free: [*mut u8; ORDERS],
}

impl<B: Break> Blocks<B> {
    /// Where the next block would be carved: the heap's top so far (0
    /// before the first).
    #[must_use]
    pub fn top(&self) -> usize {
        self.top
    }

    pub const fn new(brk: B) -> Blocks<B> {
        Blocks {
            brk,
            top: 0,
            free: [ptr::null_mut(); ORDERS],
        }
    }

    #[allow(clippy::cast_ptr_alignment)] // blocks are page-aligned
    fn push(&mut self, block: *mut u8, order: usize) {
        // SAFETY: a free block is ours and at least a page.
        unsafe { block.cast::<*mut u8>().write(self.free[order]) };
        self.free[order] = block;
    }

    #[allow(clippy::cast_ptr_alignment)] // as in `push`
    fn pop(&mut self, order: usize) -> Option<*mut u8> {
        let block = self.free[order];
        if block.is_null() {
            return None;
        }
        // SAFETY: as in `push`.
        self.free[order] = unsafe { block.cast::<*mut u8>().read() };
        Some(block)
    }

    /// The next aligned block of `order` at the break.
    fn carve(&mut self, order: usize) -> Option<*mut u8> {
        if self.top == 0 {
            self.top = self.brk.initial().checked_next_multiple_of(PAGE)?;
        }
        let size = PAGE.checked_shl(u32::try_from(order).ok()?)?;
        let start = self.top.checked_next_multiple_of(size)?;
        let end = start.checked_add(size)?;
        if !self.brk.grow(end) {
            return None;
        }
        let mut at = self.top;
        while at < start {
            let fits = (start - at) / PAGE;
            let k = ((at / PAGE).trailing_zeros()).min(fits.ilog2()) as usize;
            self.push(at as *mut u8, k);
            at += PAGE << k;
        }
        self.top = end;
        Some(start as *mut u8)
    }
}

impl<B: Break> Pages for Blocks<B> {
    fn alloc(&mut self, order: usize) -> Option<NonNull<u8>> {
        if order >= ORDERS {
            return None;
        }
        let block = match self.pop(order) {
            Some(b) => b,
            None => match (order + 1..ORDERS).find_map(|j| Some((j, self.pop(j)?))) {
                // Split: keep the lower half, free the upper, down to size.
                Some((mut j, b)) => {
                    while j > order {
                        j -= 1;
                        self.push(b.wrapping_add(PAGE << j), j);
                    }
                    b
                }
                None => self.carve(order)?,
            },
        };
        NonNull::new(block)
    }

    unsafe fn free(&mut self, block: NonNull<u8>, order: usize) {
        self.push(block.as_ptr(), order);
    }
}

/// The allocator: size classes, and blocks for what is larger.
#[derive(Debug)]
pub struct Arena<B> {
    classes: [Class; CLASSES],
    blocks: Blocks<B>,
}

impl<B: Break> Arena<B> {
    pub const fn new(brk: B) -> Arena<B> {
        Arena {
            classes: [
                Class::new(0),
                Class::new(1),
                Class::new(2),
                Class::new(3),
                Class::new(4),
                Class::new(5),
                Class::new(6),
                Class::new(7),
            ],
            blocks: Blocks::new(brk),
        }
    }

    /// The heap's top so far; see [`Blocks::top`].
    #[must_use]
    pub fn top(&self) -> usize {
        self.blocks.top()
    }

    /// Memory for `layout`, aligned to its rounded size; null if there is
    /// none.
    pub fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let got = match Size::of(layout) {
            Size::Small(i) => self.classes[i].alloc(&mut self.blocks),
            Size::Large(order) => self.blocks.alloc(order),
        };
        got.map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    /// Gives back what `alloc` gave.
    ///
    /// # Safety
    ///
    /// `p` came from `alloc(layout)` and nothing uses it any more.
    pub unsafe fn free(&mut self, p: *mut u8, layout: Layout) {
        let Some(p) = NonNull::new(p) else { return };
        match Size::of(layout) {
            // SAFETY: the caller's contract.
            Size::Small(i) => unsafe { self.classes[i].free(p, &mut self.blocks) },
            // SAFETY: as above.
            Size::Large(order) => unsafe { self.blocks.free(p, order) },
        }
    }

    /// Makes `p`, from `alloc(layout)`, `new_size` bytes: in place if the
    /// rounded size is the same, else moved.
    ///
    /// # Safety
    ///
    /// As for [`free`](Self::free), and `new_size` must make a valid
    /// layout with `layout`'s alignment.
    pub unsafe fn realloc(&mut self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        if Size::of(new) == Size::of(layout) {
            return p;
        }
        let q = self.alloc(new);
        if !q.is_null() {
            // SAFETY: both are at least the smaller size, and distinct.
            unsafe {
                ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
                self.free(p, layout);
            }
        }
        q
    }
}

/// The top of the program's heap so far: the break, as the heap has
/// moved it (0 before its first block).
#[cfg(not(test))]
#[must_use]
pub fn top() -> usize {
    global::top()
}

/// The program's heap.
#[cfg(not(test))]
mod global {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, Ordering};

    use super::{Arena, Break};

    /// The real break.
    struct Brk;

    impl Break for Brk {
        fn initial(&self) -> usize {
            #[allow(clippy::cast_possible_truncation)] // a 64-bit address space
            let end = crate::end_of_bss() as usize;
            end
        }

        fn grow(&mut self, to: usize) -> bool {
            crate::brk(to as u64).is_ok()
        }
    }

    struct Heap {
        locked: AtomicBool,
        arena: UnsafeCell<Arena<Brk>>,
    }

    // SAFETY: `arena` is used only with `locked` held.
    unsafe impl Sync for Heap {}

    impl Heap {
        fn with<T>(&self, f: impl FnOnce(&mut Arena<Brk>) -> T) -> T {
            let mut spins = 0u32;
            while self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                // Another process sharing memory holds it: let it run.
                spins += 1;
                if spins > 100 {
                    let _ = crate::sleep(0);
                } else {
                    core::hint::spin_loop();
                }
            }
            // SAFETY: the lock is held.
            let r = f(unsafe { &mut *self.arena.get() });
            self.locked.store(false, Ordering::Release);
            r
        }
    }

    pub(super) fn top() -> usize {
        HEAP.with(|a| a.top())
    }

    #[global_allocator]
    static HEAP: Heap = Heap {
        locked: AtomicBool::new(false),
        arena: UnsafeCell::new(Arena::new(Brk)),
    };

    // SAFETY: `Arena` gives memory of at least the size asked, aligned to
    // its rounded size, which is at least the alignment asked.
    unsafe impl GlobalAlloc for Heap {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            self.with(|a| a.alloc(layout))
        }

        unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
            // SAFETY: the caller's contract is `free`'s.
            self.with(|a| unsafe { a.free(p, layout) });
        }

        unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller's contract is `realloc`'s.
            self.with(|a| unsafe { a.realloc(p, layout, new_size) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A break inside a host allocation.
    struct Fake {
        base: usize,
        size: usize,
        brk: usize,
    }

    impl Fake {
        fn new(size: usize) -> Fake {
            let l = Layout::from_size_align(size, 1 << 20).unwrap();
            // SAFETY: a non-zero size; leaked, as the arena's memory is.
            let base = unsafe { std::alloc::alloc(l) } as usize;
            assert_ne!(base, 0);
            // Start off page alignment, as a bss end usually is.
            Fake {
                base,
                size,
                brk: base + 3 * PAGE + 100,
            }
        }
    }

    impl Break for Fake {
        fn initial(&self) -> usize {
            self.brk
        }

        fn grow(&mut self, to: usize) -> bool {
            if to > self.base + self.size {
                return false;
            }
            self.brk = self.brk.max(to);
            true
        }
    }

    #[test]
    fn blocks_are_aligned_and_disjoint() {
        let mut b = Blocks::new(Fake::new(64 << 20));
        let mut got = Vec::new();
        for order in [0, 3, 1, 0, 5, 2, 0, 4] {
            let p = b.alloc(order).unwrap().as_ptr() as usize;
            assert_eq!(p % (PAGE << order), 0, "order {order} misaligned");
            got.push((p, PAGE << order));
        }
        got.sort_unstable();
        for w in got.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0, "blocks overlap");
        }
    }

    #[test]
    fn gaps_and_freed_blocks_are_reused() {
        let mut b = Blocks::new(Fake::new(64 << 20));
        let big = b.alloc(4).unwrap();
        let top = b.top;
        // The gap below the order-4 block was cut up: small requests now
        // come from it, not the break.
        for _ in 0..3 {
            let p = b.alloc(0).unwrap().as_ptr() as usize;
            assert!(p < big.as_ptr() as usize);
        }
        assert_eq!(b.top, top);
        // SAFETY: from `alloc(4)`, unused.
        unsafe { b.free(big, 4) };
        assert_eq!(b.alloc(4), Some(big), "a freed block is reused");
        assert_eq!(b.top, top);
    }

    #[test]
    fn larger_free_blocks_are_split() {
        let mut b = Blocks::new(Fake::new(64 << 20));
        // At the break's first aligned 64 KiB, so the gap below is used.
        let big = b.alloc(4).unwrap();
        // SAFETY: from `alloc(4)`, unused.
        unsafe { b.free(big, 4) };
        // Empty every smaller list the gap filled, then ask for order 2.
        for k in 0..4 {
            while b.pop(k).is_some() {}
        }
        let top = b.top;
        assert_eq!(b.alloc(2), Some(big), "the lower quarter");
        assert_eq!(
            b.alloc(2).map(|p| p.as_ptr() as usize),
            Some(big.as_ptr() as usize + (PAGE << 2))
        );
        assert_eq!(
            b.alloc(3).map(|p| p.as_ptr() as usize),
            Some(big.as_ptr() as usize + (PAGE << 3))
        );
        assert_eq!(b.top, top, "all from the split block");
    }

    #[test]
    fn exhaustion_fails() {
        let mut b = Blocks::new(Fake::new(1 << 20));
        assert!(b.alloc(9).is_none(), "a 2 MiB block from 1 MiB");
        assert!(b.alloc(ORDERS).is_none());
        assert!(b.alloc(0).is_some());
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn arena_round_trips() {
        let mut a = Arena::new(Fake::new(64 << 20));
        let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
        // A deterministic mix of sizes, with frees in between.
        let mut x = 12345u32;
        for i in 0..5000u32 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let size = match x % 10 {
                0 => (x >> 8) as usize % 100_000 + 1,
                1..=3 => (x >> 8) as usize % 3000 + 1,
                _ => (x >> 8) as usize % 200 + 1,
            };
            let l = Layout::from_size_align(size, 1 << (x % 5)).unwrap();
            let p = a.alloc(l);
            assert!(!p.is_null());
            assert_eq!(p as usize % l.align(), 0);
            let tag = (i % 251) as u8;
            // SAFETY: `size` bytes were given.
            unsafe { p.write_bytes(tag, size) };
            live.push((p, l, tag));
            if x.is_multiple_of(3) {
                let (p, l, tag) = live.swap_remove((x as usize >> 4) % live.len());
                // SAFETY: as above; nothing else wrote there.
                let s = unsafe { core::slice::from_raw_parts(p, l.size()) };
                assert!(s.iter().all(|&b| b == tag), "memory overwritten");
                // SAFETY: from `alloc(l)`.
                unsafe { a.free(p, l) };
            }
        }
        for (p, l, tag) in live {
            // SAFETY: as above.
            let s = unsafe { core::slice::from_raw_parts(p, l.size()) };
            assert!(s.iter().all(|&b| b == tag), "memory overwritten");
        }
    }

    #[test]
    fn realloc_keeps_contents() {
        let mut a = Arena::new(Fake::new(64 << 20));
        let l = Layout::from_size_align(20, 8).unwrap();
        let p = a.alloc(l);
        // SAFETY: 20 bytes.
        unsafe { p.copy_from(b"twenty bytes exactly".as_ptr(), 20) };
        // SAFETY: from `alloc(l)`.
        let same = unsafe { a.realloc(p, l, 30) };
        assert_eq!(same, p, "same class, same place");
        let l30 = Layout::from_size_align(30, 8).unwrap();
        // SAFETY: as above.
        let q = unsafe { a.realloc(same, l30, 10_000) };
        assert_ne!(q, p);
        // SAFETY: at least 20 bytes copied.
        assert_eq!(unsafe { core::slice::from_raw_parts(q, 20) }, b"twenty bytes exactly");
    }
}
