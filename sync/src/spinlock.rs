use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use crate::preempt::PreemptGuard;
use slopos_arch::cpu;

/// Mutex that disables interrupts AND preemption while held.
/// Essential for kernel code accessed from both normal and interrupt contexts.
///
/// Uses a **ticket lock** internally for FIFO fairness: each acquirer takes a
/// monotonically-increasing ticket and spins until `now_serving` matches. This
/// guarantees that CPUs acquire the lock in the order they requested it,
/// eliminating starvation under SMP contention.
///
/// Supports poisoning semantics for panic recovery: after a panic-time
/// force-unlock via `poison_unlock()`, the mutex is marked poisoned.
/// Callers can check `is_poisoned()` to determine if the protected data
/// may be in an inconsistent state and needs reinitialization.
pub struct IrqMutex<T> {
    /// Monotonically-increasing ticket counter. Each `lock()` call takes the
    /// next ticket via `fetch_add(1)`. Wraps at `u16::MAX` — equality checks
    /// handle wrap-around correctly.
    next_ticket: AtomicU16,
    /// The ticket currently being served. Incremented by `fetch_add(1)` on
    /// unlock. A waiter spins until `now_serving == my_ticket`.
    now_serving: AtomicU16,
    poisoned: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: IrqMutex provides exclusive access through ticket-lock acquisition with
// interrupts and preemption disabled, making it safe to share across contexts.
unsafe impl<T: Send> Send for IrqMutex<T> {}
unsafe impl<T: Send> Sync for IrqMutex<T> {}

pub struct IrqMutexGuard<'a, T> {
    mutex: &'a IrqMutex<T>,
    saved_flags: u64,
    _preempt: PreemptGuard,
}

pub struct PreemptMutex<T> {
    next_ticket: AtomicU16,
    now_serving: AtomicU16,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for PreemptMutex<T> {}
unsafe impl<T: Send> Sync for PreemptMutex<T> {}

pub struct PreemptMutexGuard<'a, T> {
    mutex: &'a PreemptMutex<T>,
    _preempt: PreemptGuard,
}

impl<T> IrqMutex<T> {
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            next_ticket: AtomicU16::new(0),
            now_serving: AtomicU16::new(0),
            poisoned: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Force unlock the mutex without proper guard handling.
    ///
    /// Advances `now_serving` to match `next_ticket`, releasing the lock and
    /// unblocking any waiters in FIFO order.
    ///
    /// # Safety
    /// This is ONLY safe to call after a panic recovery via longjmp, when we know
    /// the lock might be held but the guard was lost. The caller must ensure:
    /// 1. No code is currently executing with this lock held
    /// 2. The data protected by the lock is in a consistent state (or will be reinitialized)
    ///
    /// Prefer `poison_unlock()` which also marks the mutex as poisoned to signal
    /// that the protected data may be in an inconsistent state.
    #[inline]
    pub unsafe fn force_unlock(&self) {
        // Snap now_serving forward to next_ticket, releasing the lock entirely.
        // This is safe under the caller's contract (no concurrent holder).
        self.now_serving
            .store(self.next_ticket.load(Ordering::Relaxed), Ordering::Release);
    }

    /// Force unlock the mutex AND mark it as poisoned.
    ///
    /// # Safety
    /// Same safety requirements as `force_unlock()`. This should be used in
    /// panic recovery paths instead of bare `force_unlock()` to signal that the
    /// protected data may be in an inconsistent state after the interrupted
    /// critical section.
    ///
    /// Callers that acquire the lock after poisoning should check `is_poisoned()`
    /// and reinitialize the protected data before trusting its invariants.
    #[inline]
    pub unsafe fn poison_unlock(&self) {
        self.poisoned.store(true, Ordering::Release);
        self.now_serving
            .store(self.next_ticket.load(Ordering::Relaxed), Ordering::Release);
    }

    /// Returns true if this mutex was force-unlocked during panic recovery.
    /// When poisoned, the protected data may be in an inconsistent state
    /// and should be reinitialized before normal use.
    #[inline]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Clear the poisoned state after the protected data has been reinitialized.
    /// Only call this after verifying or restoring the data's invariants.
    #[inline]
    pub fn clear_poison(&self) {
        self.poisoned.store(false, Ordering::Release);
    }

    /// Check if the lock is currently held (or has waiters).
    #[inline]
    pub fn is_locked(&self) -> bool {
        let next = self.next_ticket.load(Ordering::Relaxed);
        let serving = self.now_serving.load(Ordering::Relaxed);
        next != serving
    }

    #[inline]
    pub fn lock(&self) -> IrqMutexGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        // Take a ticket. fetch_add wraps at u16::MAX → 0; equality checks are
        // wrap-safe so this is correct for any number of acquisitions.
        let my_ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);

        // Spin until our ticket is being served.
        // The read of `now_serving` is Acquire so that all writes made by the
        // previous holder are visible once we observe our ticket being served.
        //
        // Proportional backoff: the further away our ticket is from now_serving,
        // the more PAUSE iterations we issue per check. This reduces cache-line
        // traffic when multiple CPUs are queued.
        loop {
            let serving = self.now_serving.load(Ordering::Acquire);
            if serving == my_ticket {
                break;
            }
            // Proportional backoff: pause 1× per ticket of distance, capped at 64.
            let distance = my_ticket.wrapping_sub(serving) as u32;
            for _ in 0..distance.min(64) {
                spin_loop();
            }
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

        // Succeed only if the lock is currently free (next_ticket == now_serving).
        // CAS next_ticket forward by 1; if someone else grabbed a ticket in the
        // meantime the CAS fails and we bail out without waiting.
        let current = self.now_serving.load(Ordering::Relaxed);
        if self
            .next_ticket
            .compare_exchange(
                current,
                current.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
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
        // Advance now_serving to hand the lock to the next waiter in FIFO order.
        // Release ordering ensures our writes are visible to the next acquirer.
        self.mutex.now_serving.fetch_add(1, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
        // _preempt drops after this, potentially triggering deferred reschedule
    }
}

impl<T> PreemptMutex<T> {
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            next_ticket: AtomicU16::new(0),
            now_serving: AtomicU16::new(0),
            data: UnsafeCell::new(data),
        }
    }

    #[inline]
    pub fn lock(&self) -> PreemptMutexGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let my_ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);

        loop {
            let serving = self.now_serving.load(Ordering::Acquire);
            if serving == my_ticket {
                break;
            }
            let distance = my_ticket.wrapping_sub(serving) as u32;
            for _ in 0..distance.min(64) {
                spin_loop();
            }
        }

        PreemptMutexGuard {
            mutex: self,
            _preempt: preempt,
        }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<PreemptMutexGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let current = self.now_serving.load(Ordering::Relaxed);
        if self
            .next_ticket
            .compare_exchange(
                current,
                current.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            Some(PreemptMutexGuard {
                mutex: self,
                _preempt: preempt,
            })
        } else {
            drop(preempt);
            None
        }
    }
}

impl<'a, T> Deref for PreemptMutexGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> DerefMut for PreemptMutexGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for PreemptMutexGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.now_serving.fetch_add(1, Ordering::Release);
    }
}

// =============================================================================
// IrqRwLock - Reader-Writer Lock with IRQ disable
// =============================================================================

/// A **writer-preferring** reader-writer lock that disables interrupts while held.
/// Multiple readers can hold the lock simultaneously, but writers get exclusive access.
/// When a writer is waiting, new readers yield to prevent writer starvation.
/// Essential for kernel data structures that need concurrent read access but exclusive writes.
pub struct IrqRwLock<T> {
    /// State: 0 = unlocked, -1 = write-locked, >0 = number of readers
    state: core::sync::atomic::AtomicI32,
    /// Number of writers waiting for access.  When > 0, new readers yield
    /// to prevent writer starvation under continuous read traffic.
    writer_waiting: AtomicU32,
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
            writer_waiting: AtomicU32::new(0),
            data: UnsafeCell::new(data),
        }
    }

    /// Acquire read access. Multiple readers can hold the lock simultaneously.
    /// Blocks if a writer holds the lock or if writers are waiting (writer preference).
    #[inline]
    pub fn read(&self) -> IrqRwLockReadGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        loop {
            let state = self.state.load(Ordering::Relaxed);
            // Yield to waiting writers: don't acquire read if a writer is queued.
            // This prevents writer starvation under continuous read traffic.
            if state >= 0 && self.writer_waiting.load(Ordering::Relaxed) == 0 {
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
    /// Fails if the lock is write-held or if writers are waiting.
    #[inline]
    pub fn try_read(&self) -> Option<IrqRwLockReadGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        let state = self.state.load(Ordering::Relaxed);
        // Respect writer preference: fail if a writer is queued.
        if state >= 0 && self.writer_waiting.load(Ordering::Relaxed) == 0 {
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
    /// Signals intent so new readers yield (writer preference).
    /// Blocks until exclusive access is available.
    #[inline]
    pub fn write(&self) -> IrqRwLockWriteGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        // Signal that a writer is waiting — new readers will yield.
        self.writer_waiting.fetch_add(1, Ordering::Relaxed);

        loop {
            // Can acquire write only if completely unlocked (state == 0)
            if self
                .state
                .compare_exchange_weak(0, -1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                // Acquired — no longer "waiting".
                self.writer_waiting.fetch_sub(1, Ordering::Relaxed);
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
