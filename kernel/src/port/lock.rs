//! Spin locks.
//!
//! A held [`SpinLock`] also holds interrupts off on its CPU (Plan 9's
//! `ilock`), so an interrupt handler can take any lock without deadlocking
//! against the code it interrupted. A cheaper lock that leaves interrupts
//! alone comes with the scheduler.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::{self, Pl};

/// A test-and-set spin lock guarding a `T`.
#[derive(Debug)]
pub(crate) struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialized by `locked`.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub(crate) const fn new(value: T) -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub(crate) fn lock(&self) -> SpinGuard<'_, T> {
        let pl = arch::splhi();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinGuard { lock: self, pl }
    }

    /// Takes the lock if it is free. Used by the panic path, which must not
    /// deadlock on a lock held by the code that panicked.
    pub(crate) fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        let pl = arch::splhi();
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SpinGuard { lock: self, pl })
        } else {
            arch::splx(pl);
            None
        }
    }

    /// Releases the lock whoever holds it.
    ///
    /// # Safety
    ///
    /// The holder must never touch the value again: only for the panic
    /// path, which does not return to the code it interrupted.
    pub(crate) unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
pub(crate) struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// The interrupt state to restore on release.
    pl: Pl,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        arch::splx(self.pl);
    }
}
