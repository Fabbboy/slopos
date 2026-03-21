//! Preemption control for SlopOS kernel.
//!
//! RAII-based preemption guards leveraging Rust's type system for compile-time safety.
//! Inspired by Linux's preempt_disable/enable and the kernel_guard crate.

use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_arch::cpu;
use slopos_arch::pcr;

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
        // SAFETY: Only accessing atomic fields on the current CPU's PCR.
        unsafe { pcr::current_pcr() }
            .preempt_count
            .fetch_add(1, Ordering::Relaxed);
        Self {
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn is_active() -> bool {
        unsafe { pcr::current_pcr() }
            .preempt_count
            .load(Ordering::Relaxed)
            > 0
    }

    #[inline]
    pub fn count() -> u32 {
        unsafe { pcr::current_pcr() }
            .preempt_count
            .load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_reschedule_pending() {
        unsafe { pcr::current_pcr() }
            .reschedule_pending
            .store(1, Ordering::Release);
    }

    #[inline]
    pub fn is_reschedule_pending() -> bool {
        unsafe { pcr::current_pcr() }
            .reschedule_pending
            .load(Ordering::Acquire)
            != 0
    }

    #[inline]
    pub fn clear_reschedule_pending() {
        unsafe { pcr::current_pcr() }
            .reschedule_pending
            .store(0, Ordering::Release);
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
        // SAFETY: Only accessing atomic fields on the current CPU's PCR.
        let pcr = unsafe { pcr::current_pcr() };
        let prev = pcr.preempt_count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "preempt_count underflow");

        if prev == 1 && pcr.reschedule_pending.swap(0, Ordering::AcqRel) != 0 {
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
