use core::arch::asm;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::cpu;

/// Minimal spinlock helper with IRQ save/restore (matches legacy C semantics).
pub struct Spinlock {
    locked: AtomicBool,
}

/// Mutex that disables interrupts while held. Essential for kernel code that
/// may be accessed from both normal context and interrupt handlers.
///
/// Unlike `spin::Mutex`, this mutex saves RFLAGS and disables interrupts on
/// lock acquisition, preventing deadlocks when an interrupt fires while the
/// lock is held.
pub struct IrqMutex<T> {
    lock: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: IrqMutex provides exclusive access through atomic locking with
// interrupts disabled, making it safe to share across contexts.
unsafe impl<T: Send> Send for IrqMutex<T> {}
unsafe impl<T: Send> Sync for IrqMutex<T> {}

/// RAII guard for IrqMutex. Restores interrupt state on drop.
pub struct IrqMutexGuard<'a, T> {
    mutex: &'a IrqMutex<T>,
    saved_flags: u64,
}

impl<T> IrqMutex<T> {
    /// Create a new interrupt-safe mutex.
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            lock: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Acquire the lock, disabling interrupts. Returns a guard that releases
    /// the lock and restores interrupt state on drop.
    ///
    /// # Safety Note
    /// Interrupts remain disabled while spinning for the lock. This prevents
    /// nested interrupt storms that can cause stack overflow when IRQ handlers
    /// contend for locks (e.g., rapid keyboard input causing nested IRQs that
    /// each push ~500 bytes onto the stack).
    #[inline]
    pub fn lock(&self) -> IrqMutexGuard<'_, T> {
        // Save flags and disable interrupts BEFORE trying to acquire lock
        let saved_flags = read_rflags();
        cpu::disable_interrupts();

        // Spin to acquire the lock with interrupts disabled.
        // DO NOT re-enable interrupts while spinning - this prevents nested
        // interrupt storms that can overflow the kernel stack.
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
        }
    }

    /// Try to acquire the lock without blocking. Returns None if already held.
    #[inline]
    pub fn try_lock(&self) -> Option<IrqMutexGuard<'_, T>> {
        let saved_flags = read_rflags();
        cpu::disable_interrupts();

        if self
            .lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(IrqMutexGuard {
                mutex: self,
                saved_flags,
            })
        } else {
            // Failed to acquire - restore interrupt state
            if saved_flags & (1 << 9) != 0 {
                cpu::enable_interrupts();
            }
            None
        }
    }
}

impl<'a, T> Deref for IrqMutexGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: We hold the lock exclusively
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> DerefMut for IrqMutexGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: We hold the lock exclusively
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for IrqMutexGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // Release the lock
        self.mutex.lock.store(false, Ordering::Release);
        // Restore interrupt state
        if self.saved_flags & (1 << 9) != 0 {
            cpu::enable_interrupts();
        }
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

    /// Acquire lock and disable interrupts, returning prior RFLAGS.
    #[inline(always)]
    pub fn lock_irqsave(&self) -> u64 {
        let flags = read_rflags();
        cpu::disable_interrupts();
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }
        flags
    }

    /// Release lock and restore IF bit from saved RFLAGS.
    #[inline(always)]
    pub fn unlock_irqrestore(&self, flags: u64) {
        self.locked.store(false, Ordering::Release);
        if flags & (1 << 9) != 0 {
            cpu::enable_interrupts();
        }
    }
}

#[inline(always)]
fn read_rflags() -> u64 {
    let flags: u64;
    unsafe {
        asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags));
    }
    flags
}
