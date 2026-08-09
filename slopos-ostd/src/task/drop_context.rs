//! Context checks and helpers for destructors that may return memory to the
//! kernel allocator.

use crate::cpu::x86_64::interrupts;
use crate::sync::lock_tracking::{held_lock_count, held_lock_snapshot};

/// Whether preemption is disabled on this CPU.
///
/// The kernel reads the PCR field directly, which is a single gs-relative load
/// but faults anywhere GS_BASE is not a control region. Host builds have no
/// PCR, so they read the backend counter they actually maintain — the same
/// split `IrqDisabled` makes for RFLAGS.
#[inline]
fn preemption_disabled() -> bool {
    #[cfg(target_os = "none")]
    {
        crate::cpu::preempt::PreemptGuard::is_active()
    }
    #[cfg(not(target_os = "none"))]
    {
        crate::cpu::preempt::is_preempt_disabled()
    }
}

/// Whether the calling context may run a destructor that returns memory to the
/// allocator.
///
/// Three facts about the calling CPU right now, never anything about a
/// reference count: a count-based test cannot be made race-free, and these can.
/// Interrupts must be on and no tracked lock held because freeing takes the
/// allocator's own locks, and taking them from inside another lock or with
/// interrupts masked is how the allocator — where every subsystem meets —
/// becomes an ordering hazard for all of them. Preemption must be enabled
/// because a caller holding a whole-sequence guard is asking for the region to
/// stay free of destructors, and because the dying task's stack is pinned to
/// the dispatching CPU until the switch completes.
///
/// Both the deferral decision and the tripwire that catches it being wrong read
/// this, so the two cannot drift apart.
#[inline]
pub fn drop_context_is_safe() -> bool {
    interrupts::are_interrupts_enabled() && held_lock_count() == 0 && !preemption_disabled()
}

/// Assert that a task destructor is running in a context where allocator and
/// synchronous TLB-reclaim work may safely execute.
#[inline]
pub fn assert_task_drop_context() {
    debug_assert!(
        interrupts::are_interrupts_enabled(),
        "Task dropped with interrupts disabled"
    );
    // One observation: this runs preemptible, so asking the count and the
    // name separately can name a lock the caller never held.
    let (held, innermost) = held_lock_snapshot();
    debug_assert!(
        held == 0,
        "Task dropped while the current CPU holds a tracked lock: {:?}",
        innermost
    );
}

/// Run `operation` after verifying the context a deferred drop needs.
///
/// The assertions are [`drop_context_is_safe`] taken apart, so each names which
/// fact failed; the predicate is what the deferral decision reads, and this is
/// what catches it having been read wrong.
#[inline]
pub fn run_off_lock<R>(operation: impl FnOnce() -> R) -> R {
    debug_assert!(
        interrupts::are_interrupts_enabled(),
        "deferred drop attempted with interrupts disabled"
    );
    let (held, innermost) = held_lock_snapshot();
    debug_assert!(
        held == 0,
        "deferred drop attempted while the current CPU holds a tracked lock: {:?}",
        innermost
    );
    debug_assert!(
        !preemption_disabled(),
        "deferred drop attempted with preemption disabled"
    );
    operation()
}

/// Drop `value` through [`run_off_lock`].
#[inline]
pub fn drop_off_lock<T>(value: T) {
    run_off_lock(|| drop(value));
}
