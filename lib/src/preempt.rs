//! Preemption control for SlopOS kernel.
//!
//! RAII-based preemption guards leveraging Rust's type system for compile-time safety.
//! Inspired by Linux's preempt_disable/enable and the kernel_guard crate.

use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use crate::cpu;
use crate::percpu::get_percpu_data;

static RESCHEDULE_PENDING: AtomicU32 = AtomicU32::new(0);
static RESCHEDULE_CALLBACK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// RAII guard that disables preemption while held.
/// Guards are nestable - preemption re-enables only when all guards drop.
/// !Send/!Sync: must stay on same CPU context.
#[must_use = "if unused, preemption will be immediately re-enabled"]
pub struct PreemptGuard {
    _marker: PhantomData<*mut ()>,
}

impl PreemptGuard {
    #[inline]
    pub fn new() -> Self {
        let percpu = get_percpu_data();
        percpu.preempt_count.fetch_add(1, Ordering::Relaxed);
        Self {
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn is_active() -> bool {
        get_percpu_data().preempt_count.load(Ordering::Relaxed) > 0
    }

    #[inline]
    pub fn count() -> u32 {
        get_percpu_data().preempt_count.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_reschedule_pending() {
        RESCHEDULE_PENDING.store(1, Ordering::SeqCst);
    }

    #[inline]
    pub fn is_reschedule_pending() -> bool {
        RESCHEDULE_PENDING.load(Ordering::SeqCst) != 0
    }

    #[inline]
    pub fn clear_reschedule_pending() {
        RESCHEDULE_PENDING.store(0, Ordering::SeqCst);
    }
}

impl Default for PreemptGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PreemptGuard {
    #[inline]
    fn drop(&mut self) {
        let percpu = get_percpu_data();
        let prev = percpu.preempt_count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "preempt_count underflow");

        if prev == 1 && RESCHEDULE_PENDING.swap(0, Ordering::SeqCst) != 0 {
            let fn_ptr = RESCHEDULE_CALLBACK.load(Ordering::Acquire);
            if !fn_ptr.is_null() {
                // SAFETY: fn_ptr was set via register_reschedule_callback with a valid fn()
                let callback: fn() = unsafe { core::mem::transmute(fn_ptr) };
                callback();
            }
        }
    }
}

/// Combined IRQ-disable + Preemption-disable guard.
/// On drop: restore flags, then preempt guard drops (may trigger deferred reschedule).
#[must_use = "if unused, protection will be immediately released"]
pub struct IrqPreemptGuard {
    saved_flags: u64,
    _preempt: PreemptGuard,
}

impl IrqPreemptGuard {
    #[inline]
    pub fn new() -> Self {
        let saved_flags = cpu::save_flags_cli();
        Self {
            saved_flags,
            _preempt: PreemptGuard::new(),
        }
    }

    #[inline]
    pub fn saved_flags(&self) -> u64 {
        self.saved_flags
    }
}

impl Default for IrqPreemptGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IrqPreemptGuard {
    #[inline]
    fn drop(&mut self) {
        // Restore flags first. _preempt drops after this body completes,
        // which is correct: reschedule callback runs with interrupts enabled.
        cpu::restore_flags(self.saved_flags);
    }
}

pub fn register_reschedule_callback(callback: fn()) {
    RESCHEDULE_CALLBACK.store(callback as *mut (), Ordering::Release);
}

#[inline]
pub fn is_preemption_disabled() -> bool {
    PreemptGuard::is_active()
}

#[inline]
pub fn preempt_count() -> u32 {
    PreemptGuard::count()
}
