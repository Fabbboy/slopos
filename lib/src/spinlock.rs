use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::cpu;
use crate::preempt::PreemptGuard;

pub struct Spinlock {
    locked: AtomicBool,
}

/// Mutex that disables interrupts AND preemption while held.
/// Essential for kernel code accessed from both normal and interrupt contexts.
pub struct IrqMutex<T> {
    lock: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: IrqMutex provides exclusive access through atomic locking with
// interrupts and preemption disabled, making it safe to share across contexts.
unsafe impl<T: Send> Send for IrqMutex<T> {}
unsafe impl<T: Send> Sync for IrqMutex<T> {}

pub struct IrqMutexGuard<'a, T> {
    mutex: &'a IrqMutex<T>,
    saved_flags: u64,
    _preempt: PreemptGuard,
}

impl<T> IrqMutex<T> {
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            lock: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    #[inline]
    pub fn lock(&self) -> IrqMutexGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }

        IrqMutexGuard {
            mutex: self,
            saved_flags,
            _preempt: preempt,
        }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<IrqMutexGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        if self
            .lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(IrqMutexGuard {
                mutex: self,
                saved_flags,
                _preempt: preempt,
            })
        } else {
            cpu::restore_flags(saved_flags);
            drop(preempt);
            None
        }
    }
}

impl<'a, T> Deref for IrqMutexGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> DerefMut for IrqMutexGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for IrqMutexGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.lock.store(false, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
        // _preempt drops after this, potentially triggering deferred reschedule
    }
}

impl Spinlock {
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }

    #[inline(always)]
    pub fn init(&self) {
        self.locked.store(false, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn lock(&self) {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }
    }

    #[inline(always)]
    pub fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }

    #[inline(always)]
    pub fn lock_irqsave(&self) -> u64 {
        let flags = cpu::save_flags_cli();
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }
        flags
    }

    #[inline(always)]
    pub fn unlock_irqrestore(&self, flags: u64) {
        self.locked.store(false, Ordering::Release);
        cpu::restore_flags(flags);
    }
}
