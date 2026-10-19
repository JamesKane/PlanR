//! The kernel heap: `lib/heap`'s size classes over the frame allocator,
//! behind `#[global_allocator]`, so `alloc`'s `Box`, `Vec` and `Arc` work.
//!
//! Each class has its own lock, taken before the frame allocator's when a
//! class needs a slab. Blocks larger than a class come from the frame
//! allocator directly. Nothing is allocated before [`mm::init`]: until
//! then every request fails.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};

use bootinfo::{HHDM_BASE, phys_to_virt};
use heap::{CLASSES, Class, Pages, Size};

use crate::port::lock::SpinLock;
use crate::port::mm;

/// Slabs and large blocks, from [`mm`], reached through the direct map.
#[derive(Debug)]
struct Frames;

impl Pages for Frames {
    fn alloc(&mut self, order: usize) -> Option<NonNull<u8>> {
        let phys = mm::alloc_frames(order)?;
        #[allow(clippy::cast_possible_truncation)] // 64-bit targets
        NonNull::new(ptr::with_exposed_provenance_mut(phys_to_virt(phys) as usize))
    }

    unsafe fn free(&mut self, block: NonNull<u8>, order: usize) {
        let phys = block.as_ptr().expose_provenance() as u64 - HHDM_BASE;
        // SAFETY: the block came from `alloc(order)`, and the caller is done
        // with it.
        unsafe { mm::free_frames(phys, order) };
    }
}

#[derive(Debug)]
struct Heap {
    classes: [SpinLock<Class>; CLASSES],
}

const _: () = assert!(CLASSES == 8);

#[global_allocator]
static HEAP: Heap = Heap {
    classes: [
        SpinLock::new(Class::new(0)),
        SpinLock::new(Class::new(1)),
        SpinLock::new(Class::new(2)),
        SpinLock::new(Class::new(3)),
        SpinLock::new(Class::new(4)),
        SpinLock::new(Class::new(5)),
        SpinLock::new(Class::new(6)),
        SpinLock::new(Class::new(7)),
    ],
};

// SAFETY: classes and frame blocks hand out distinct memory of at least the
// rounded size, aligned to it, and `Size::of` rounds up to the alignment.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let got = match Size::of(layout) {
            Size::Small(i) => self.classes[i].lock().alloc(&mut Frames),
            Size::Large(order) => Frames.alloc(order),
        };
        got.map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: `alloc` returned `p` for this layout, so it is not null.
        let p = unsafe { NonNull::new_unchecked(p) };
        match Size::of(layout) {
            // SAFETY: the same layout picks the same class or order, and the
            // caller is done with the memory.
            Size::Small(i) => unsafe { self.classes[i].lock().free(p, &mut Frames) },
            // SAFETY: as above.
            Size::Large(order) => unsafe { Frames.free(p, order) },
        }
    }
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use alloc::alloc::{alloc, dealloc};
    use alloc::boxed::Box;
    use alloc::format;
    use alloc::vec::Vec;
    use core::alloc::Layout;

    use crate::port::ktest::TestResult;
    use crate::port::mm;

    /// `Box`, `Vec` and `format!` allocate, grow and keep their contents.
    pub(crate) fn collections_work() -> TestResult {
        let v: Vec<u64> = (0..10_000).collect();
        if v.iter().sum::<u64>() != 10_000 * 9_999 / 2 {
            return Err("Vec contents lost".into());
        }
        let b = Box::new([0x5au8; 100]);
        if b.iter().any(|&x| x != 0x5a) {
            return Err("Box contents lost".into());
        }
        if format!("{}-{:x}", 42, 255) != "42-ff" {
            return Err("format! wrong".into());
        }
        Ok(())
    }

    /// Once everything allocated is freed, the slabs go back to the frame
    /// allocator.
    pub(crate) fn slabs_return() -> TestResult {
        // Warm the class up, so its kept slab is already counted.
        drop(Box::new([0u8; 64]));
        let before = mm::frame_counts().0;
        let boxes: Vec<Box<[u8; 64]>> = (0..1000).map(|_| Box::new([1; 64])).collect();
        if mm::frame_counts().0 >= before {
            return Err("no slabs taken".into());
        }
        drop(boxes);
        if mm::frame_counts().0 != before {
            return Err("slabs not returned".into());
        }
        Ok(())
    }

    /// A large, strongly aligned request gets a block of its own.
    pub(crate) fn large_aligned() -> TestResult {
        let layout = Layout::from_size_align(40_000, 64 * 1024).map_err(|_| "bad layout")?;
        let before = mm::frame_counts().0;
        // SAFETY: the layout is not empty.
        let p = unsafe { alloc(layout) };
        if p.is_null() {
            return Err("no memory".into());
        }
        let aligned = p.addr().is_multiple_of(64 * 1024);
        // SAFETY: allocated just above with this layout.
        unsafe { dealloc(p, layout) };
        if !aligned {
            return Err("not aligned".into());
        }
        if mm::frame_counts().0 != before {
            return Err("block not returned".into());
        }
        Ok(())
    }
}
