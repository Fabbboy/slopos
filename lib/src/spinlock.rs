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

// =============================================================================
// IrqRwLock - Reader-Writer Lock with IRQ disable
// =============================================================================

/// A reader-writer lock that disables interrupts while held.
/// Multiple readers can hold the lock simultaneously, but writers get exclusive access.
/// Essential for kernel data structures that need concurrent read access but exclusive writes.
pub struct IrqRwLock<T> {
    /// State: 0 = unlocked, -1 = write-locked, >0 = number of readers
    state: core::sync::atomic::AtomicI32,
    data: UnsafeCell<T>,
}

// SAFETY: IrqRwLock provides synchronized access through atomic operations with
// interrupts disabled, making it safe to share across contexts.
unsafe impl<T: Send> Send for IrqRwLock<T> {}
unsafe impl<T: Send + Sync> Sync for IrqRwLock<T> {}

/// Guard for read access to IrqRwLock data.
pub struct IrqRwLockReadGuard<'a, T> {
    lock: &'a IrqRwLock<T>,
    saved_flags: u64,
    _preempt: PreemptGuard,
}

/// Guard for write access to IrqRwLock data.
pub struct IrqRwLockWriteGuard<'a, T> {
    lock: &'a IrqRwLock<T>,
    saved_flags: u64,
    _preempt: PreemptGuard,
}

impl<T> IrqRwLock<T> {
    /// Create a new IrqRwLock protecting the given data.
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            state: core::sync::atomic::AtomicI32::new(0),
            data: UnsafeCell::new(data),
        }
    }

    /// Acquire read access. Multiple readers can hold the lock simultaneously.
    /// Blocks if a writer holds the lock.
    #[inline]
    pub fn read(&self) -> IrqRwLockReadGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        loop {
            let state = self.state.load(Ordering::Relaxed);
            // Can acquire read if no writer (state >= 0)
            if state >= 0 {
                if self
                    .state
                    .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return IrqRwLockReadGuard {
                        lock: self,
                        saved_flags,
                        _preempt: preempt,
                    };
                }
            }
            spin_loop();
        }
    }

    /// Try to acquire read access without blocking.
    #[inline]
    pub fn try_read(&self) -> Option<IrqRwLockReadGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        let state = self.state.load(Ordering::Relaxed);
        if state >= 0 {
            if self
                .state
                .compare_exchange(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Some(IrqRwLockReadGuard {
                    lock: self,
                    saved_flags,
                    _preempt: preempt,
                });
            }
        }
        cpu::restore_flags(saved_flags);
        drop(preempt);
        None
    }

    /// Acquire write access. Only one writer can hold the lock, and no readers.
    /// Blocks until exclusive access is available.
    #[inline]
    pub fn write(&self) -> IrqRwLockWriteGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        loop {
            // Can acquire write only if completely unlocked (state == 0)
            if self
                .state
                .compare_exchange_weak(0, -1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return IrqRwLockWriteGuard {
                    lock: self,
                    saved_flags,
                    _preempt: preempt,
                };
            }
            spin_loop();
        }
    }

    /// Try to acquire write access without blocking.
    #[inline]
    pub fn try_write(&self) -> Option<IrqRwLockWriteGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        if self
            .state
            .compare_exchange(0, -1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Some(IrqRwLockWriteGuard {
                lock: self,
                saved_flags,
                _preempt: preempt,
            });
        }
        cpu::restore_flags(saved_flags);
        drop(preempt);
        None
    }
}

impl<'a, T> Deref for IrqRwLockReadGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: Read guard ensures no writers, data is valid
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> Drop for IrqRwLockReadGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.state.fetch_sub(1, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
        // _preempt drops after this
    }
}

impl<'a, T> Deref for IrqRwLockWriteGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: Write guard ensures exclusive access
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> DerefMut for IrqRwLockWriteGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: Write guard ensures exclusive access
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<'a, T> Drop for IrqRwLockWriteGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.state.store(0, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
        // _preempt drops after this
    }
}

// =============================================================================
// Spinlock - Basic spinlock without RAII guard
// =============================================================================

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
