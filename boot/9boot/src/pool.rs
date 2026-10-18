//! A global allocator over UEFI pool memory, usable until boot services are
//! exited. Allocation after that returns null (and so panics in `alloc`).

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicPtr, Ordering};

use uefi::{BootServices, MemoryType, raw};

static BOOT_SERVICES: AtomicPtr<raw::BootServices> = AtomicPtr::new(null_mut());

/// Enables allocation through `bs`.
pub(crate) fn enable(bs: &BootServices<'_>) {
    BOOT_SERVICES.store(bs.as_raw().cast_mut(), Ordering::Release);
}

/// Disables allocation; called just before `ExitBootServices`.
pub(crate) fn disable() {
    BOOT_SERVICES.store(null_mut(), Ordering::Release);
}

fn boot_services() -> Option<BootServices<'static>> {
    let p = BOOT_SERVICES.load(Ordering::Acquire);
    // SAFETY: only set while boot services are active (`enable`/`disable`).
    (!p.is_null()).then(|| unsafe { BootServices::from_raw(p) })
}

struct Pool;

/// Each block is over-allocated so the returned pointer can be aligned, and
/// the pool pointer is stored in the 8 bytes just before it.
const HEADER: usize = 8;

// SAFETY: blocks come from AllocatePool and are returned with FreePool.
unsafe impl GlobalAlloc for Pool {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(bs) = boot_services() else { return null_mut() };
        let align = layout.align().max(HEADER);
        let Ok(base) = bs.allocate_pool(MemoryType::LoaderData, layout.size() + align + HEADER) else {
            return null_mut();
        };
        let user = (base.as_ptr() as usize + HEADER).next_multiple_of(align);
        // SAFETY: `user - HEADER .. user + size` lies inside the block.
        unsafe { ((user - HEADER) as *mut *mut u8).write(base.as_ptr()) };
        user as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // After ExitBootServices the memory is reclaimed wholesale; leak.
        let Some(bs) = boot_services() else { return };
        // SAFETY: `alloc` stored the pool pointer just before `ptr`.
        let base = unsafe { ((ptr as usize - HEADER) as *const *mut u8).read() };
        if let Some(base) = NonNull::new(base) {
            // SAFETY: `base` came from allocate_pool.
            let _ = unsafe { bs.free_pool(base) };
        }
    }
}

#[global_allocator]
static POOL: Pool = Pool;
