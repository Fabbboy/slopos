//! Sequence lock for read-dominated data.
//!
//! Readers take no lock: they sample an even sequence counter, copy the
//! value, and retry if the counter moved. Writers are exclusive and rare.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

use crate::cpu::preempt::PreemptGuard;
use crate::cpu::x86_64 as cpu;

/// A sequence lock protecting a small `Copy` value. Readers are wait-free in
/// the uncontended case and retry only when racing with a writer.
pub struct SeqLock<T> {
    /// Monotonic sequence counter. Even = stable, odd = write in progress.
    seq: AtomicU32,
    /// Writer mutual exclusion: the sequence counter alone does not stop two
    /// CPUs writing simultaneously on SMP.
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
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            seq: AtomicU32::new(0),
            writer_lock: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Read a consistent snapshot of the protected value, retrying while a
    /// writer is active.
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

    /// Acquire exclusive write access. IRQs and preemption stay disabled
    /// until the returned guard drops.
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

        self.seq.fetch_add(1, Ordering::Release);

        SeqLockWriteGuard {
            lock: self,
            saved_flags,
            _preempt: preempt,
        }
    }

    /// The raw sequence counter, for debugging and stats.
    #[inline]
    pub fn sequence(&self) -> u32 {
        self.seq.load(Ordering::Relaxed)
    }
}

impl<'a, T: Copy> SeqLockWriteGuard<'a, T> {
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: we hold writer_lock; no concurrent reader can observe a
        // consistent snapshot until we drop and the seq counter goes even.
        unsafe { &mut *self.lock.data.get() }
    }

    #[inline]
    pub fn write(&mut self, value: T) {
        // SAFETY: see `get_mut`.
        unsafe { *self.lock.data.get() = value };
    }
}

impl<T: Copy> Drop for SeqLockWriteGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.seq.fetch_add(1, Ordering::Release);

        self.lock.writer_lock.store(false, Ordering::Release);

        // `_preempt` drops after this, so any reschedule it triggers runs with
        // the caller's IRQ state already restored.
        cpu::restore_flags(self.saved_flags);
    }
}
