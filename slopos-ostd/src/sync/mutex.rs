//! Sleeping mutex built on top of [`SpinLock`] + [`WaitQueue`].
//!
//! Distinct from [`SpinLock<T>`](super::spin::SpinLock) in that contended
//! lockers block via the wait queue rather than spinning. Suitable for
//! protecting state that may be held across long-running operations
//! (file I/O, network round-trips). Not suitable for IRQ-context use —
//! attempting to lock from an interrupt handler would block the CPU.
//!
//! Until a [`WaitQueueBackend`](super::wait_queue::WaitQueueBackend) is
//! registered, `lock()` falls back to spin-acquiring the inner spinlock
//! repeatedly (no blocking surface available).

use crate::sync::lock_tracking::LockClassKey;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::mm::AllocError;
use crate::mm::init::{Init, init_from_closure, init_from_owned};
use crate::sync::wait_queue::{WaitAbort, WaitQueue, WaitResult};

/// A sleeping mutex.
pub struct Mutex<T> {
    /// `true` while the mutex is held.
    locked: AtomicBool,
    /// Wait queue used to park contended lockers.
    waiters: WaitQueue,
    data: UnsafeCell<T>,
}

// SAFETY: synchronisation through `locked` + the wait queue.
unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Create a new mutex protecting the given data.
    ///
    /// `class` names the inner wait queue, which is the tracked lock here —
    /// the `Mutex` itself sleeps and so cannot live on the per-CPU held
    /// stack.
    pub const fn new(data: T, class: &'static LockClassKey) -> Self {
        Self {
            locked: AtomicBool::new(false),
            waiters: WaitQueue::new(class),
            data: UnsafeCell::new(data),
        }
    }

    /// Place an already-owned `data` directly into the destination.
    ///
    /// `KArc::try_new(Mutex::new(big, class))` copies `big` twice — once
    /// into the `Mutex`, once into the allocation — and both copies are
    /// stack frames. This writes it to the heap slot once. The error type
    /// is fixed rather than generic so consumer crates never have to name
    /// `AllocError`, which is `allocator_api`-unstable.
    pub fn init_owned(data: T, class: &'static LockClassKey) -> impl Init<Self, AllocError> {
        Self::init_with(class, init_from_owned::<T, AllocError>(data))
    }

    /// In-place [`Init`] recipe, so a large `T` never materialises on the
    /// caller's stack between allocation and construction. Used via
    /// `KArc::try_init(Mutex::init_with(class, T::init_…()))`.
    pub fn init_with<E>(
        class: &'static LockClassKey,
        data_init: impl Init<T, E>,
    ) -> impl Init<Self, E>
    where
        E: From<AllocError>,
    {
        // SAFETY: the closure writes every field of `slot`. The `locked`
        // and `waiters` shapes replicate `Self::new`'s byte pattern by
        // hand so no `Self` rvalue is built; `data_init.__init` writes the
        // inner `T` straight into the same heap slot.
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

    /// Try to acquire the lock without blocking.
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
    /// death while contending. A dying task must return through its own frames
    /// rather than park on a lock whose holder may itself be dying: this type
    /// is released only by `MutexGuard::drop`, which never runs on a stack
    /// nobody unwinds, so a task abandoned here holds the lock forever.
    ///
    /// With no wait-queue backend registered — the pre-scheduler device-probe
    /// paths — there is no blocking surface at all and the acquire degrades to
    /// busy-waiting on the flag.
    ///
    /// Killable rather than interruptible: a `SIGINT` must not abandon a
    /// filesystem operation midway.
    #[must_use = "an unacquired lock guards nothing"]
    pub fn lock(&self) -> WaitResult<MutexGuard<'_, T>> {
        loop {
            if let Some(guard) = self.try_lock() {
                return Ok(guard);
            }

            // Slow path: enqueue under inner lock, then block until a
            // waker calls `wake_one`. The condition closure re-checks the
            // atomic flag so we don't park if the holder released between
            // our CAS attempt above and the enqueue.
            match self
                .waiters
                .wait_event(|| !self.locked.load(Ordering::Acquire))
            {
                Ok(()) => {}
                Err(abort @ (WaitAbort::Killed | WaitAbort::Interrupted)) => return Err(abort),
                // `Timeout` cannot arise on an untimed wait; treating it as a
                // spin rather than a panic keeps this path panic-free.
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// Consume the mutex and return the inner data.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

/// RAII guard returned by [`Mutex::lock`] / [`Mutex::try_lock`].
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: We hold the lock; exclusive access guaranteed.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: We hold the lock; exclusive access guaranteed.
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
