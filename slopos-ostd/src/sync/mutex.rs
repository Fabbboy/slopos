//! Sleeping mutex built on top of [`SpinLock`] + [`WaitQueue`].
//!
//! Contended lockers block on the wait queue rather than spinning, so the lock
//! may be held across long-running operations — and must never be taken from an
//! interrupt handler, which would block the CPU.
//!
//! Until a [`WaitQueueBackend`](super::wait_queue::WaitQueueBackend) is
//! registered, `lock()` falls back to spin-acquiring the inner spinlock.

use crate::sync::lock_tracking::LockClassKey;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::mm::AllocError;
use crate::mm::init::{Init, init_from_closure, init_from_owned};
use crate::sync::wait_queue::{WaitAbort, WaitQueue, WaitResult};

pub struct Mutex<T> {
    locked: AtomicBool,
    waiters: WaitQueue,
    data: UnsafeCell<T>,
}

// SAFETY: synchronisation through `locked` + the wait queue.
unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// `class` names the inner wait queue, which is the tracked lock here: the
    /// `Mutex` itself sleeps and so cannot live on the per-CPU held stack.
    pub const fn new(data: T, class: &'static LockClassKey) -> Self {
        Self {
            locked: AtomicBool::new(false),
            waiters: WaitQueue::new(class),
            data: UnsafeCell::new(data),
        }
    }

    /// Place an already-owned `data` directly into the destination:
    /// `KArc::try_new(Mutex::new(big, class))` would stage `big` through two
    /// stack frames. The error type is fixed rather than generic so consumer
    /// crates never have to name `AllocError`, which is
    /// `allocator_api`-unstable.
    pub fn init_owned(data: T, class: &'static LockClassKey) -> impl Init<Self, AllocError> {
        Self::init_with(class, init_from_owned::<T, AllocError>(data))
    }

    /// In-place [`Init`] recipe, so a large `T` never materialises on the
    /// caller's stack between allocation and construction.
    pub fn init_with<E>(
        class: &'static LockClassKey,
        data_init: impl Init<T, E>,
    ) -> impl Init<Self, E>
    where
        E: From<AllocError>,
    {
        // SAFETY: the closure writes every field of `slot`; `locked` and
        // `waiters` are built in place so no `Self` rvalue exists, and
        // `data_init` writes the inner `T` into the same heap slot.
        unsafe {
            init_from_closure(move |slot: *mut Self| -> Result<(), E> {
                addr_of_mut!((*slot).locked).write(AtomicBool::new(false));
                addr_of_mut!((*slot).waiters).write(WaitQueue::new(class));
                let data_ptr = addr_of_mut!((*slot).data) as *mut T;
                data_init.__init(data_ptr)?;
                Ok(())
            })
        }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(MutexGuard { mutex: self })
        } else {
            None
        }
    }

    /// Acquire the lock, blocking the current task if necessary.
    ///
    /// Fails with [`WaitAbort::Killed`] when the current task is marked for
    /// death while contending: the guard is released only by its `Drop`, so a
    /// task abandoned here would hold the lock forever. Killable rather than
    /// interruptible — a `SIGINT` must not abandon a filesystem operation
    /// midway.
    ///
    /// With no wait-queue backend registered (the pre-scheduler device-probe
    /// paths) the acquire degrades to busy-waiting on the flag.
    #[must_use = "an unacquired lock guards nothing"]
    pub fn lock(&self) -> WaitResult<MutexGuard<'_, T>> {
        loop {
            if let Some(guard) = self.try_lock() {
                return Ok(guard);
            }

            match self
                .waiters
                .wait_event(|| !self.locked.load(Ordering::Acquire))
            {
                Ok(()) => {}
                Err(abort @ (WaitAbort::Killed | WaitAbort::Interrupted)) => return Err(abort),
                // `Timeout` cannot arise on an untimed wait; spinning rather
                // than panicking keeps this path panic-free.
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock, so access is exclusive.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock, so access is exclusive.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
        self.mutex.waiters.wake_one();
    }
}
