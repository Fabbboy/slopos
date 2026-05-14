//! Ticket-lock spinning mutex / rwlock primitives.
//!
//! [`SpinLock<T>`] disables both interrupts and preemption while held — the
//! workhorse mutex for kernel data accessed from both task and interrupt
//! contexts. Internally it is a **ticket lock**: each acquirer takes a
//! monotonically-increasing ticket and spins until `now_serving` matches,
//! guaranteeing FIFO acquisition order even under heavy SMP contention.
//!
//! [`PreemptMutex<T>`] is the same shape but only disables preemption (does
//! not save/restore RFLAGS). Use it when the lock is never taken from an
//! IRQ handler.
//!
//! [`IrqRwLock<T>`] is a writer-preferring reader-writer lock that disables
//! interrupts while held. New readers yield to a queued writer to prevent
//! writer starvation.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use crate::cpu::preempt::PreemptGuard;
use crate::cpu::x86_64 as cpu;
use crate::mm::AllocError;
use crate::mm::init::{Init, init_from_closure};
use crate::sync::lock_tracking;

/// Ticket-lock mutex that disables interrupts AND preemption while held.
/// Essential for kernel data accessed from both normal and interrupt contexts.
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
pub struct SpinLock<T> {
    /// Monotonically-increasing ticket counter. Each `lock()` call takes the
    /// next ticket via `fetch_add(1)`. Wraps at `u16::MAX` — equality checks
    /// handle wrap-around correctly.
    next_ticket: AtomicU16,
    /// The ticket currently being served. Incremented by `fetch_add(1)` on
    /// unlock. A waiter spins until `now_serving == my_ticket`.
    now_serving: AtomicU16,
    poisoned: AtomicBool,
    /// Lock ordering level for deadlock prevention. Acquiring a lock at
    /// level N while holding a lock at level >= N is a violation.
    level: u8,
    data: UnsafeCell<T>,
}

// SAFETY: SpinLock provides exclusive access through ticket-lock acquisition with
// interrupts and preemption disabled, making it safe to share across contexts.
unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

pub struct SpinLockGuard<'a, T> {
    mutex: &'a SpinLock<T>,
    saved_flags: u64,
    _preempt: PreemptGuard,
}

pub struct PreemptMutex<T> {
    next_ticket: AtomicU16,
    now_serving: AtomicU16,
    level: u8,
    data: UnsafeCell<T>,
}

// SAFETY: PreemptMutex provides exclusive access via ticket lock with
// preemption disabled.
unsafe impl<T: Send> Send for PreemptMutex<T> {}
unsafe impl<T: Send> Sync for PreemptMutex<T> {}

pub struct PreemptMutexGuard<'a, T> {
    mutex: &'a PreemptMutex<T>,
    _preempt: PreemptGuard,
}

impl<T> SpinLock<T> {
    #[inline]
    pub const fn new(data: T, level: u8) -> Self {
        Self {
            next_ticket: AtomicU16::new(0),
            now_serving: AtomicU16::new(0),
            poisoned: AtomicBool::new(false),
            level,
            data: UnsafeCell::new(data),
        }
    }

    /// In-place [`Init`] recipe: build the lock fields directly into
    /// the heap slot, threading the caller's `data_init` recipe through
    /// to the inner `UnsafeCell<T>`. Lets large `T` (e.g. a 256-slot
    /// timer wheel) avoid materialising on the caller's stack between
    /// allocation and the `SpinLock::new(data, ...)` call. Used via
    /// `KBox::try_init(SpinLock::init_with(level, T::init_default()))`.
    pub fn init_with<E>(level: u8, data_init: impl Init<T, E>) -> impl Init<Self, E>
    where
        E: From<AllocError>,
    {
        // SAFETY: the closure writes every field of `slot`. Atomic-init
        // and `UnsafeCell::new` shape are fixed by the `SpinLock`
        // layout — we replicate the byte pattern of `Self::new(data, ..)`
        // by hand so no `Self` rvalue ever materialises on the stack.
        // `data_init.__init` writes the inner `T` directly into the
        // same heap slot via `addr_of_mut!((*slot).data) as *mut T`.
        unsafe {
            init_from_closure(move |slot: *mut Self| -> Result<(), E> {
                addr_of_mut!((*slot).next_ticket).write(AtomicU16::new(0));
                addr_of_mut!((*slot).now_serving).write(AtomicU16::new(0));
                addr_of_mut!((*slot).poisoned).write(AtomicBool::new(false));
                addr_of_mut!((*slot).level).write(level);
                let data_ptr = addr_of_mut!((*slot).data) as *mut T;
                data_init.__init(data_ptr)?;
                Ok(())
            })
        }
    }

    /// Returns the lock ordering level.
    #[inline]
    pub const fn level(&self) -> u8 {
        self.level
    }

    /// Force unlock the mutex without proper guard handling.
    ///
    /// # Safety
    /// This is ONLY safe to call after a panic recovery via longjmp, when we know
    /// the lock might be held but the guard was lost.
    #[inline]
    pub unsafe fn force_unlock(&self) {
        self.now_serving
            .store(self.next_ticket.load(Ordering::Relaxed), Ordering::Release);
    }

    /// Force unlock the mutex AND mark it as poisoned.
    ///
    /// # Safety
    /// Same safety requirements as `force_unlock()`. Use in panic recovery
    /// paths to signal that the protected data may be inconsistent.
    #[inline]
    pub unsafe fn poison_unlock(&self) {
        self.poisoned.store(true, Ordering::Release);
        self.now_serving
            .store(self.next_ticket.load(Ordering::Relaxed), Ordering::Release);
    }

    /// Returns true if this mutex was force-unlocked during panic recovery.
    #[inline]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Clear the poisoned state after the protected data has been reinitialized.
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

    /// Get a raw pointer to the protected data without taking the lock.
    ///
    /// # Safety
    /// The caller must ensure that accessing the data through this pointer
    /// is safe. Typical use: reading naturally-aligned fields that are
    /// written atomically.
    #[inline]
    pub unsafe fn as_ptr(&self) -> *const T {
        self.data.get() as *const T
    }

    /// Read a naturally-aligned field of the protected data without
    /// taking the lock. The closure `f` is restricted to receiving a
    /// shared reference to the inner `T`; correctness relies on the
    /// caller's discipline that every field accessed through `f` is a
    /// naturally-aligned scalar (u32 / pointer / atomic) which is only
    /// written under the lock, so a plain load is tear-free on x86-64.
    /// Composite (multi-word) fields MUST re-acquire the lock via
    /// [`Self::lock`] instead.
    ///
    /// Folds the one `unsafe { &*self.as_ptr() }` reborrow interior to
    /// OSTD so consumer crates' lock-free field-peek helpers stay in
    /// safe Rust.
    #[inline]
    pub fn read_atomic_field<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        // SAFETY: the closure contract above restricts `f` to tear-free
        // reads of naturally-aligned fields that are only written under
        // the lock; the resulting shared borrow does not outlive `f`'s
        // execution.
        let inner: &T = unsafe { &*self.data.get() };
        f(inner)
    }

    #[inline]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        // Take a ticket. fetch_add wraps at u16::MAX → 0; equality checks are
        // wrap-safe so this is correct for any number of acquisitions.
        let my_ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);

        // Spin until our ticket is being served.
        // Proportional backoff: the further away our ticket is from now_serving,
        // the more PAUSE iterations we issue per check.
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

        // SAFETY: Preemption is disabled, self is a static lock.
        unsafe {
            lock_tracking::push_lock(
                self as *const _ as *const (),
                spinlock_poison_fn::<T>,
                self.level,
            );
        }

        SpinLockGuard {
            mutex: self,
            saved_flags,
            _preempt: preempt,
        }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

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
            // SAFETY: Preemption is disabled, self is a static lock.
            unsafe {
                lock_tracking::push_lock(
                    self as *const _ as *const (),
                    spinlock_poison_fn::<T>,
                    self.level,
                );
            }
            Some(SpinLockGuard {
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

impl<'a, T> Deref for SpinLockGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: We hold the lock; exclusive access guaranteed.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> DerefMut for SpinLockGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: We hold the lock; exclusive access guaranteed.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for SpinLockGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Lock is currently held by this guard.
        unsafe {
            lock_tracking::pop_lock(self.mutex as *const _ as *const ());
        }
        self.mutex.now_serving.fetch_add(1, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
    }
}

/// Poison-unlock callback for lock tracking.
///
/// # Safety
/// `addr` must point to a live `SpinLock<T>`.
unsafe fn spinlock_poison_fn<T>(addr: *const ()) {
    // SAFETY: caller certifies addr is a valid SpinLock<T>.
    let mutex = unsafe { &*(addr as *const SpinLock<T>) };
    // SAFETY: panic-recovery contract.
    unsafe { mutex.poison_unlock() };
}

impl<T> PreemptMutex<T> {
    #[inline]
    pub const fn new(data: T, level: u8) -> Self {
        Self {
            next_ticket: AtomicU16::new(0),
            now_serving: AtomicU16::new(0),
            level,
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

        // SAFETY: Preemption is disabled, self is a static lock.
        unsafe {
            lock_tracking::push_lock(
                self as *const _ as *const (),
                preempt_mutex_poison_fn::<T>,
                self.level,
            );
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
            // SAFETY: Preemption is disabled, self is a static lock.
            unsafe {
                lock_tracking::push_lock(
                    self as *const _ as *const (),
                    preempt_mutex_poison_fn::<T>,
                    self.level,
                );
            }
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
        // SAFETY: We hold the lock.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> DerefMut for PreemptMutexGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: We hold the lock.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for PreemptMutexGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Lock is currently held by this guard.
        unsafe {
            lock_tracking::pop_lock(self.mutex as *const _ as *const ());
        }
        self.mutex.now_serving.fetch_add(1, Ordering::Release);
    }
}

/// Poison-unlock callback for PreemptMutex lock tracking.
///
/// # Safety
/// `addr` must point to a live `PreemptMutex<T>`.
unsafe fn preempt_mutex_poison_fn<T>(addr: *const ()) {
    // SAFETY: caller certifies addr is a valid PreemptMutex<T>.
    let mutex = unsafe { &*(addr as *const PreemptMutex<T>) };
    mutex
        .now_serving
        .store(mutex.next_ticket.load(Ordering::Relaxed), Ordering::Release);
}

// =============================================================================
// IrqRwLock - Reader-Writer Lock with IRQ disable
// =============================================================================

/// A **writer-preferring** reader-writer lock that disables interrupts while held.
/// Multiple readers can hold the lock simultaneously, but writers get exclusive access.
/// When a writer is waiting, new readers yield to prevent writer starvation.
pub struct IrqRwLock<T> {
    /// State: 0 = unlocked, -1 = write-locked, >0 = number of readers
    state: core::sync::atomic::AtomicI32,
    /// Number of writers waiting for access. When > 0, new readers yield
    /// to prevent writer starvation under continuous read traffic.
    writer_waiting: AtomicU32,
    level: u8,
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
    pub const fn new(data: T, level: u8) -> Self {
        Self {
            state: core::sync::atomic::AtomicI32::new(0),
            writer_waiting: AtomicU32::new(0),
            level,
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
            if state >= 0 && self.writer_waiting.load(Ordering::Relaxed) == 0 {
                if self
                    .state
                    .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    // SAFETY: Preemption is disabled, self is a static lock.
                    unsafe {
                        lock_tracking::push_lock(
                            self as *const _ as *const (),
                            irq_rwlock_poison_fn::<T>,
                            self.level,
                        );
                    }
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
        if state >= 0 && self.writer_waiting.load(Ordering::Relaxed) == 0 {
            if self
                .state
                .compare_exchange(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                // SAFETY: Preemption is disabled, self is a static lock.
                unsafe {
                    lock_tracking::push_lock(
                        self as *const _ as *const (),
                        irq_rwlock_poison_fn::<T>,
                        self.level,
                    );
                }
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

    /// Acquire write access. Only one writer, no readers. Signals intent so
    /// new readers yield (writer preference).
    #[inline]
    pub fn write(&self) -> IrqRwLockWriteGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        self.writer_waiting.fetch_add(1, Ordering::Relaxed);

        loop {
            if self
                .state
                .compare_exchange_weak(0, -1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                self.writer_waiting.fetch_sub(1, Ordering::Relaxed);
                // SAFETY: Preemption is disabled, self is a static lock.
                unsafe {
                    lock_tracking::push_lock(
                        self as *const _ as *const (),
                        irq_rwlock_poison_fn::<T>,
                        self.level,
                    );
                }
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
            // SAFETY: Preemption is disabled, self is a static lock.
            unsafe {
                lock_tracking::push_lock(
                    self as *const _ as *const (),
                    irq_rwlock_poison_fn::<T>,
                    self.level,
                );
            }
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
        // SAFETY: Read guard ensures no writers, data is valid.
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> Drop for IrqRwLockReadGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Lock is currently held by this guard.
        unsafe {
            lock_tracking::pop_lock(self.lock as *const _ as *const ());
        }
        self.lock.state.fetch_sub(1, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
    }
}

impl<'a, T> Deref for IrqRwLockWriteGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: Write guard ensures exclusive access.
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> DerefMut for IrqRwLockWriteGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: Write guard ensures exclusive access.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<'a, T> Drop for IrqRwLockWriteGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Lock is currently held by this guard.
        unsafe {
            lock_tracking::pop_lock(self.lock as *const _ as *const ());
        }
        self.lock.state.store(0, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
    }
}

/// Poison-unlock callback for IrqRwLock lock tracking.
///
/// # Safety
/// `addr` must point to a live `IrqRwLock<T>`.
unsafe fn irq_rwlock_poison_fn<T>(addr: *const ()) {
    // SAFETY: caller certifies addr is a valid IrqRwLock<T>.
    let lock = unsafe { &*(addr as *const IrqRwLock<T>) };
    lock.state.store(0, Ordering::Release);
}
