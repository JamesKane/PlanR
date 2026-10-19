//! The kernel heap: power-of-two size classes carved from slabs, and whole
//! page blocks for anything larger.
//!
//! A request is rounded up to a power of two no smaller than its alignment
//! ([`Size::of`]). Up to [`MAX_SMALL`] bytes it comes from a [`Class`]:
//! objects of one size packed into *slabs*. Each slab is one block from a
//! [`Pages`] source, aligned to its own size, so the slab an object
//! belongs to is found by masking the object's address, and its [`Slab`]
//! header sits at the start of the block. Anything larger is a block of
//! its own straight from the source.
//!
//! A class keeps the slabs that have free objects on a list. A full slab
//! is on no list, and an empty one goes back to the source unless it is
//! the class's last, which saves churn at the boundary.
//!
//! Nothing here locks; the kernel holds one lock per class.

#![cfg_attr(not(test), no_std)]

use core::alloc::Layout;
use core::ptr::{self, NonNull};

/// The page size.
pub const PAGE: usize = 4096;
/// The smallest object.
pub const MIN: usize = 16;
/// The number of size classes: [`MIN`] to [`MAX_SMALL`] bytes.
pub const CLASSES: usize = 8;
/// The largest object a class holds.
pub const MAX_SMALL: usize = MIN << (CLASSES - 1);
/// The fewest objects a slab is made for (less its header).
const OBJECTS_PER_SLAB: usize = 8;

/// `p` as a pointer to a `T`, which every object and slab is aligned for.
#[allow(clippy::cast_ptr_alignment)] // objects are aligned to their size, at least MIN
fn at<T>(p: *mut u8) -> *mut T {
    debug_assert!(p.addr().is_multiple_of(align_of::<T>()));
    p.cast()
}

/// Where slabs and large blocks come from.
pub trait Pages {
    /// `2^order` pages, aligned to their size.
    fn alloc(&mut self, order: usize) -> Option<NonNull<u8>>;

    /// Returns a block `alloc` gave.
    ///
    /// # Safety
    ///
    /// `block` came from `alloc(order)` and nothing uses it any more.
    unsafe fn free(&mut self, block: NonNull<u8>, order: usize);
}

/// How a request is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// From the class with this index.
    Small(usize),
    /// From a block of `2^order` pages.
    Large(usize),
}

impl Size {
    /// How `layout` is served.
    #[must_use]
    pub fn of(layout: Layout) -> Size {
        let n = layout.size().max(layout.align()).max(MIN).next_power_of_two();
        if n <= MAX_SMALL {
            Size::Small((n / MIN).trailing_zeros() as usize)
        } else {
            Size::Large((n / PAGE).max(1).trailing_zeros() as usize)
        }
    }
}

/// The header at the start of a slab.
#[repr(C)]
#[derive(Debug)]
pub struct Slab {
    next: *mut Slab,
    prev: *mut Slab,
    /// The first free object.
    free: *mut Free,
    /// Objects handed out.
    used: usize,
}

/// A free object's first word.
#[derive(Debug)]
struct Free {
    next: *mut Free,
}

/// One size class.
#[derive(Debug)]
pub struct Class {
    size: usize,
    /// Slabs with a free object.
    partial: *mut Slab,
    slabs: usize,
    used: usize,
}

// SAFETY: the pointers lead into slabs this class owns; whoever shares a
// `Class` between CPUs serializes access to it.
unsafe impl Send for Class {}

impl Class {
    /// The class with index `index` ([`Size::Small`]).
    #[must_use]
    pub const fn new(index: usize) -> Class {
        assert!(index < CLASSES);
        Class {
            size: MIN << index,
            partial: ptr::null_mut(),
            slabs: 0,
            used: 0,
        }
    }

    /// The size of its objects.
    #[must_use]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Slabs held, and objects handed out.
    #[must_use]
    pub fn counts(&self) -> (usize, usize) {
        (self.slabs, self.used)
    }

    /// The order of this class's slabs.
    #[must_use]
    pub fn slab_order(&self) -> usize {
        (self.size * OBJECTS_PER_SLAB / PAGE).max(1).trailing_zeros() as usize
    }

    fn slab_bytes(&self) -> usize {
        PAGE << self.slab_order()
    }

    /// Where a slab's first object is: past the header, on an object
    /// boundary.
    fn first(&self) -> usize {
        size_of::<Slab>().next_multiple_of(self.size)
    }

    /// Objects in one slab.
    #[must_use]
    pub fn per_slab(&self) -> usize {
        (self.slab_bytes() - self.first()) / self.size
    }

    /// An object of [`size`](Class::size) bytes, aligned to its size. Its
    /// contents are whatever they were.
    pub fn alloc(&mut self, pages: &mut impl Pages) -> Option<NonNull<u8>> {
        if self.partial.is_null() {
            self.grow(pages)?;
        }
        let slab = self.partial;
        // SAFETY: a slab on the list is live and has a free object.
        unsafe {
            let obj = (*slab).free;
            (*slab).free = (*obj).next;
            (*slab).used += 1;
            if (*slab).free.is_null() {
                self.unlink(slab);
            }
            self.used += 1;
            Some(NonNull::new_unchecked(obj.cast()))
        }
    }

    /// Returns an object.
    ///
    /// # Safety
    ///
    /// `obj` came from this class's [`alloc`](Class::alloc), with the same
    /// `pages`, and nothing uses it any more.
    pub unsafe fn free(&mut self, obj: NonNull<u8>, pages: &mut impl Pages) {
        let mask = self.slab_bytes() - 1;
        let slab = at::<Slab>(obj.as_ptr().map_addr(|a| a & !mask));
        let obj = at::<Free>(obj.as_ptr());
        // SAFETY: the object's slab is live, and the object is ours again.
        unsafe {
            let was_full = (*slab).free.is_null();
            obj.write(Free { next: (*slab).free });
            (*slab).free = obj;
            (*slab).used -= 1;
            self.used -= 1;
            if was_full {
                self.push(slab);
            }
            // Keep the last slab, so one object coming and going does not
            // fetch and return a slab each time.
            if (*slab).used == 0 && !((*slab).next.is_null() && (*slab).prev.is_null()) {
                self.unlink(slab);
                self.slabs -= 1;
                pages.free(NonNull::new_unchecked(slab.cast()), self.slab_order());
            }
        }
    }

    /// Adds a new slab to the partial list.
    fn grow(&mut self, pages: &mut impl Pages) -> Option<()> {
        let block = pages.alloc(self.slab_order())?.as_ptr();
        let first = self.first();
        let n = self.per_slab();
        let slab = at::<Slab>(block);
        // SAFETY: the block is new and ours; the header and the objects lie
        // inside it, each object aligned to its size.
        unsafe {
            let mut free = ptr::null_mut::<Free>();
            for i in (0..n).rev() {
                let obj = at::<Free>(block.add(first + i * self.size));
                obj.write(Free { next: free });
                free = obj;
            }
            slab.write(Slab {
                next: ptr::null_mut(),
                prev: ptr::null_mut(),
                free,
                used: 0,
            });
            self.push(slab);
        }
        self.slabs += 1;
        Some(())
    }

    /// # Safety
    ///
    /// `slab` is live and on no list.
    unsafe fn push(&mut self, slab: *mut Slab) {
        // SAFETY: the caller's contract, and the list's slabs are live.
        unsafe {
            (*slab).prev = ptr::null_mut();
            (*slab).next = self.partial;
            if let Some(next) = self.partial.as_mut() {
                next.prev = slab;
            }
        }
        self.partial = slab;
    }

    /// # Safety
    ///
    /// `slab` is on the partial list.
    unsafe fn unlink(&mut self, slab: *mut Slab) {
        // SAFETY: the caller's contract, and the list's slabs are live.
        unsafe {
            let (next, prev) = ((*slab).next, (*slab).prev);
            match prev.as_mut() {
                Some(p) => p.next = next,
                None => self.partial = next,
            }
            if let Some(n) = next.as_mut() {
                n.prev = prev;
            }
            (*slab).next = ptr::null_mut();
            (*slab).prev = ptr::null_mut();
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_ptr_alignment)] // test patterns; objects are aligned
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Blocks from the host allocator, counted.
    #[derive(Default)]
    struct Host {
        out: HashMap<usize, usize>,
    }

    impl Pages for Host {
        fn alloc(&mut self, order: usize) -> Option<NonNull<u8>> {
            let layout = Layout::from_size_align(PAGE << order, PAGE << order).unwrap();
            // SAFETY: the layout is not empty.
            let p = NonNull::new(unsafe { std::alloc::alloc(layout) })?;
            self.out.insert(p.as_ptr().addr(), order);
            Some(p)
        }

        unsafe fn free(&mut self, block: NonNull<u8>, order: usize) {
            assert_eq!(self.out.remove(&block.as_ptr().addr()), Some(order), "foreign block");
            let layout = Layout::from_size_align(PAGE << order, PAGE << order).unwrap();
            // SAFETY: allocated above with this layout.
            unsafe { std::alloc::dealloc(block.as_ptr(), layout) };
        }
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn sizes_round_to_classes_and_orders() {
        assert_eq!(Size::of(layout(1, 1)), Size::Small(0));
        assert_eq!(Size::of(layout(16, 8)), Size::Small(0));
        assert_eq!(Size::of(layout(17, 8)), Size::Small(1));
        assert_eq!(Size::of(layout(8, 256)), Size::Small(4));
        assert_eq!(Size::of(layout(2048, 8)), Size::Small(7));
        assert_eq!(Size::of(layout(2049, 8)), Size::Large(0));
        assert_eq!(Size::of(layout(4097, 8)), Size::Large(1));
        assert_eq!(Size::of(layout(8, 1 << 20)), Size::Large(8));
    }

    #[test]
    fn slabs_hold_several_objects() {
        for i in 0..CLASSES {
            let c = Class::new(i);
            assert!(
                c.per_slab() >= OBJECTS_PER_SLAB - 1,
                "class {} holds {}",
                c.size(),
                c.per_slab()
            );
            assert_eq!(c.first() % c.size(), 0);
        }
    }

    #[test]
    fn objects_aligned_distinct_and_writable() {
        let mut host = Host::default();
        for i in 0..CLASSES {
            let mut c = Class::new(i);
            let n = c.per_slab() * 3 + 1;
            let objs: Vec<NonNull<u8>> = (0..n).map(|_| c.alloc(&mut host).unwrap()).collect();
            for (k, o) in objs.iter().enumerate() {
                assert_eq!(o.as_ptr().addr() % c.size(), 0);
                // SAFETY: each object is `size` bytes and ours.
                unsafe { o.as_ptr().write_bytes(k as u8, c.size()) };
            }
            for (k, o) in objs.iter().enumerate() {
                // SAFETY: as above.
                let bytes = unsafe { std::slice::from_raw_parts(o.as_ptr(), c.size()) };
                assert!(
                    bytes.iter().all(|&b| b == k as u8),
                    "class {} object {k} overwritten",
                    c.size()
                );
            }
            assert_eq!(c.counts(), (4, n));
            for o in objs {
                // SAFETY: from this class, no longer used.
                unsafe { c.free(o, &mut host) };
            }
            assert_eq!(c.counts(), (1, 0), "all but the last slab returned");
        }
        assert_eq!(host.out.len(), CLASSES);
    }

    #[test]
    fn churn_keeps_accounts() {
        let mut host = Host::default();
        let mut c = Class::new(2);
        let mut live = Vec::new();
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..50_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if seed % 5 < 3 || live.is_empty() {
                let o = c.alloc(&mut host).unwrap();
                // SAFETY: the object is ours; tag it with its own address.
                unsafe { o.as_ptr().cast::<usize>().write(o.as_ptr().addr()) };
                live.push(o);
            } else {
                let o = live.swap_remove((seed >> 20) as usize % live.len());
                // SAFETY: as tagged above.
                assert_eq!(unsafe { o.as_ptr().cast::<usize>().read() }, o.as_ptr().addr());
                // SAFETY: from this class, no longer used.
                unsafe { c.free(o, &mut host) };
            }
            assert_eq!(c.counts().1, live.len());
            assert_eq!(c.counts().0, host.out.len());
        }
        for o in live {
            // SAFETY: as above.
            unsafe { c.free(o, &mut host) };
        }
        assert_eq!(c.counts(), (1, 0));
    }
}
