//! Locks that sleep: [`QLock`] (Plan 9's `QLock`) and [`RwLock`]
//! (`RWlock`), as RAII guards.
//!
//! Taking one needs a [`Sleepable`]: only a thread with no spin lock held
//! may wait for it. A contended lock queues its waiters in order and
//! hands itself straight to the next one on release, so no waiter is
//! overtaken.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

use crate::port::lock::SpinLock;
use crate::port::proc::{self, Proc, Sleepable};

/// A sleeping mutual-exclusion lock guarding a `T`.
#[derive(Debug)]
pub(crate) struct QLock<T> {
    state: SpinLock<QState>,
    value: UnsafeCell<T>,
}

#[derive(Debug)]
struct QState {
    held: bool,
    waiters: VecDeque<Arc<Proc>>,
}

// SAFETY: access to `value` is serialized by `held`.
unsafe impl<T: Send> Sync for QLock<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Send for QLock<T> {}

impl<T> QLock<T> {
    pub(crate) const fn new(value: T) -> QLock<T> {
        QLock {
            state: SpinLock::new(QState {
                held: false,
                waiters: VecDeque::new(),
            }),
            value: UnsafeCell::new(value),
        }
    }

    /// Takes the lock, sleeping until it is free.
    pub(crate) fn lock(&self, s: &Sleepable) -> QGuard<'_, T> {
        let mut st = self.state.lock();
        if !st.held {
            st.held = true;
            return QGuard { lock: self };
        }
        drop(st);
        // Queue under the lock, rechecking: it may have come free.
        proc::sleep_queued(s, |me| {
            let mut st = self.state.lock();
            if st.held {
                st.waiters.push_back(me);
            } else {
                st.held = true;
                drop(st);
                proc::ready(me);
            }
        });
        // Woken by `unlock`, which handed the lock over (or took it above).
        QGuard { lock: self }
    }

    /// Takes the lock if it is free.
    pub(crate) fn try_lock(&self) -> Option<QGuard<'_, T>> {
        let mut st = self.state.lock();
        if st.held {
            return None;
        }
        st.held = true;
        Some(QGuard { lock: self })
    }

    fn unlock(&self) {
        let mut st = self.state.lock();
        match st.waiters.pop_front() {
            // The lock stays held, now by `next`.
            Some(next) => {
                drop(st);
                proc::ready(next);
            }
            None => st.held = false,
        }
    }
}

/// A held [`QLock`].
#[derive(Debug)]
pub(crate) struct QGuard<'a, T> {
    lock: &'a QLock<T>,
}

impl<T> Deref for QGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for QGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for QGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}

/// A sleeping readers-writer lock guarding a `T`. Waiters are served in
/// order: a reader arriving behind a waiting writer waits for it.
#[derive(Debug)]
pub(crate) struct RwLock<T> {
    state: SpinLock<RwState>,
    value: UnsafeCell<T>,
}

#[derive(Debug)]
struct RwState {
    readers: usize,
    writer: bool,
    /// Each waiter and whether it wants to write.
    waiters: VecDeque<(Arc<Proc>, bool)>,
}

impl RwState {
    /// Hands the lock to the waiters at the front that can have it: one
    /// writer, or every reader up to the next writer. Returns them, to be
    /// made ready once the state lock is dropped.
    fn grant(&mut self) -> VecDeque<Arc<Proc>> {
        let mut go = VecDeque::new();
        if self.writer || self.readers > 0 {
            return go;
        }
        if let Some((_, true)) = self.waiters.front() {
            self.writer = true;
            go.extend(self.waiters.pop_front().map(|(p, _)| p));
            return go;
        }
        while let Some((_, false)) = self.waiters.front() {
            self.readers += 1;
            go.extend(self.waiters.pop_front().map(|(p, _)| p));
        }
        go
    }
}

// SAFETY: `value` is shared only between readers, or held by one writer.
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Send for RwLock<T> {}

impl<T> RwLock<T> {
    pub(crate) const fn new(value: T) -> RwLock<T> {
        RwLock {
            state: SpinLock::new(RwState {
                readers: 0,
                writer: false,
                waiters: VecDeque::new(),
            }),
            value: UnsafeCell::new(value),
        }
    }

    /// Takes a shared hold, sleeping while a writer holds or waits.
    pub(crate) fn read(&self, s: &Sleepable) -> ReadGuard<'_, T> {
        self.acquire(s, false);
        ReadGuard { lock: self }
    }

    /// Takes the exclusive hold, sleeping until no one else holds it.
    pub(crate) fn write(&self, s: &Sleepable) -> WriteGuard<'_, T> {
        self.acquire(s, true);
        WriteGuard { lock: self }
    }

    fn acquire(&self, s: &Sleepable, write: bool) {
        {
            let mut st = self.state.lock();
            let free = !st.writer && st.waiters.is_empty() && (!write || st.readers == 0);
            if free {
                if write {
                    st.writer = true;
                } else {
                    st.readers += 1;
                }
                return;
            }
        }
        proc::sleep_queued(s, |me| {
            let mut st = self.state.lock();
            st.waiters.push_back((me, write));
            // It may have come free since the check above.
            let go = st.grant();
            drop(st);
            go.into_iter().for_each(proc::ready);
        });
    }

    fn release(&self, write: bool) {
        let mut st = self.state.lock();
        if write {
            st.writer = false;
        } else {
            st.readers -= 1;
        }
        let go = st.grant();
        drop(st);
        go.into_iter().for_each(proc::ready);
    }
}

/// A shared hold on a [`RwLock`].
#[derive(Debug)]
pub(crate) struct ReadGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: no writer holds the lock while we do.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release(false);
    }
}

/// The exclusive hold on a [`RwLock`].
#[derive(Debug)]
pub(crate) struct WriteGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock alone.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock alone.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release(true);
    }
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::{QLock, RwLock};
    use crate::port::clock;
    use crate::port::cpu;
    use crate::port::ktest::TestResult;
    use crate::port::proc::{Spawn, sleepable, spawn, yield_now};

    const SECOND: u64 = 1_000_000_000;

    fn threads() -> usize {
        cpu::online().count() * 2 + 1
    }

    /// Contending threads, yielding inside the lock, never lose an update.
    pub(crate) fn qlock_excludes() -> TestResult {
        const ROUNDS: usize = 200;
        let lock = Arc::new(QLock::new(0usize));
        let done = Arc::new(AtomicUsize::new(0));
        let n = threads();
        for _ in 0..n {
            let (lock, done) = (lock.clone(), done.clone());
            spawn(Spawn::kproc("qlock"), move || {
                let s = sleepable().expect("a thread may sleep");
                for _ in 0..ROUNDS {
                    let mut g = lock.lock(&s);
                    let v = *g;
                    yield_now();
                    *g = v + 1;
                }
                done.fetch_add(1, Ordering::Release);
            })
            .ok_or("cannot spawn")?;
        }
        if !clock::wait_until(|| done.load(Ordering::Acquire) == n, 10 * SECOND) {
            return Err("threads did not finish within 10 s".into());
        }
        if lock.try_lock().map(|g| *g) != Some(n * ROUNDS) {
            return Err("updates lost".into());
        }
        Ok(())
    }

    /// Readers share the lock, a writer has it alone, and both progress.
    pub(crate) fn rwlock_excludes() -> TestResult {
        const ROUNDS: usize = 100;
        /// Readers in, and writers in, as seen inside the lock.
        struct Inside {
            readers: AtomicUsize,
            writers: AtomicUsize,
            bad: AtomicUsize,
        }
        let lock = Arc::new(RwLock::new(Inside {
            readers: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
            bad: AtomicUsize::new(0),
        }));
        let done = Arc::new(AtomicUsize::new(0));
        let n = threads();
        for i in 0..n {
            let (lock, done) = (lock.clone(), done.clone());
            spawn(Spawn::kproc("rwlock"), move || {
                let s = sleepable().expect("a thread may sleep");
                for _ in 0..ROUNDS {
                    if i % 3 == 0 {
                        let g = lock.write(&s);
                        if g.writers.fetch_add(1, Ordering::AcqRel) != 0 || g.readers.load(Ordering::Acquire) != 0 {
                            g.bad.fetch_add(1, Ordering::Relaxed);
                        }
                        yield_now();
                        g.writers.fetch_sub(1, Ordering::AcqRel);
                    } else {
                        let g = lock.read(&s);
                        g.readers.fetch_add(1, Ordering::AcqRel);
                        if g.writers.load(Ordering::Acquire) != 0 {
                            g.bad.fetch_add(1, Ordering::Relaxed);
                        }
                        yield_now();
                        g.readers.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                done.fetch_add(1, Ordering::Release);
            })
            .ok_or("cannot spawn")?;
        }
        if !clock::wait_until(|| done.load(Ordering::Acquire) == n, 10 * SECOND) {
            return Err("threads did not finish within 10 s".into());
        }
        // SAFETY: every thread is done with the lock.
        let bad = unsafe { &*lock.value.get() }.bad.load(Ordering::Relaxed);
        if bad != 0 {
            return Err("a reader and a writer were inside together".into());
        }
        Ok(())
    }
}
