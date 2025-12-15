use core::arch::asm;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::cpu;

/// Minimal spinlock helper with IRQ save/restore (matches legacy C semantics).
pub struct Spinlock {
    locked: AtomicBool,
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
