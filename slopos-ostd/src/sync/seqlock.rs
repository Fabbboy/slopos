//! Sequence lock for read-dominated data.
//!
//! A `SeqLock<T>` allows any number of concurrent readers with **zero
//! synchronization overhead** (no atomics, no locks on the read path beyond a
//! sequence-counter check). Writers are exclusive and rare.
//!
//! # How it works
//!
//! A monotonically-increasing sequence counter tracks writer activity:
//! - **Even** → data is stable, readers may proceed.
//! - **Odd**  → a writer is mid-update, readers must retry.
//!
//! Writers acquire a spinlock for mutual exclusion (the sequence counter
//! alone does NOT exclude concurrent writers on SMP), then bump the counter
//! to odd. On exit, the counter goes even and the spinlock is released.
//!
//! # When to use
//!
//! - Data is small and `Copy` (read via register-width loads).
//! - Reads vastly outnumber writes.
//! - Readers can tolerate retrying on the rare writer race.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

use crate::cpu::preempt::PreemptGuard;
use slopos_arch::cpu;

/// A sequence lock protecting a small `Copy` value.
///
/// Writers are mutually exclusive via an internal spinlock (SMP-safe).
/// Readers are wait-free in the uncontended case and retry only when
/// racing with a writer.
pub struct SeqLock<T> {
    /// Monotonic sequence counter. Even = stable, odd = write in progress.
    seq: AtomicU32,
    /// Writer mutual exclusion. The sequence counter alone does NOT prevent
    /// two CPUs from writing simultaneously — this spinlock does.
    writer_lock: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: Writers hold exclusive access via spinlock + IRQ/preempt disable.
// Readers only ever read a Copy snapshot and retry on inconsistency.
unsafe impl<T: Send> Send for SeqLock<T> {}
unsafe impl<T: Send + Sync> Sync for SeqLock<T> {}

/// RAII guard returned by [`SeqLock::write_lock`].
pub struct SeqLockWriteGuard<'a, T: Copy> {
    lock: &'a SeqLock<T>,
    saved_flags: u64,
    _preempt: PreemptGuard,
}

impl<T: Copy> SeqLock<T> {
    /// Create a new sequence lock with the given initial value.
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            seq: AtomicU32::new(0),
            writer_lock: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Read a consistent snapshot of the protected value.
    ///
    /// **Lock-free** in the uncontended case. If a writer is active,
    /// the reader retries with a spin-loop hint.
    #[inline]
    pub fn read(&self) -> T {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);

            if s1 & 1 != 0 {
                spin_loop();
                continue;
            }

            // SAFETY: T is Copy, and we verify consistency below.
            let value = unsafe { *self.data.get() };

            fence(Ordering::Acquire);

            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 == s2 {
                return value;
            }

            spin_loop();
        }
    }

    /// Acquire exclusive write access.
    ///
    /// Disables IRQs and preemption, acquires the writer spinlock, then
    /// bumps the sequence counter to odd. The returned guard provides
    /// `&mut T` access and restores state on drop.
    #[inline]
    pub fn write_lock(&self) -> SeqLockWriteGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        while self
            .writer_lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.writer_lock.load(Ordering::Relaxed) {
                spin_loop();
            }
        }

        // Bump sequence to odd → readers will retry.
        self.seq.fetch_add(1, Ordering::Release);

        SeqLockWriteGuard {
            lock: self,
            saved_flags,
            _preempt: preempt,
        }
    }

    /// Read the raw sequence counter (for debugging / stats).
    #[inline]
    pub fn sequence(&self) -> u32 {
        self.seq.load(Ordering::Relaxed)
    }
}

impl<'a, T: Copy> SeqLockWriteGuard<'a, T> {
    /// Get a mutable reference to the protected data.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: we hold writer_lock; no concurrent reader can observe a
        // consistent snapshot until we drop and the seq counter goes even.
        unsafe { &mut *self.lock.data.get() }
    }

    /// Overwrite the protected data entirely.
    #[inline]
    pub fn write(&mut self, value: T) {
        // SAFETY: see `get_mut`.
        unsafe { *self.lock.data.get() = value };
    }
}

impl<T: Copy> Drop for SeqLockWriteGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // Bump sequence to even → readers will see stable data.
        self.lock.seq.fetch_add(1, Ordering::Release);

        // Release writer spinlock.
        self.lock.writer_lock.store(false, Ordering::Release);

        // Restore IRQs. _preempt drops after this, possibly triggering reschedule.
        cpu::restore_flags(self.saved_flags);
    }
}
