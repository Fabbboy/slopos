//! Context checks and helpers for destructors that may return memory to the
//! kernel allocator.

use crate::cpu::x86_64::interrupts;
use crate::sync::lock_tracking::held_lock_snapshot;

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

/// Run `operation` after verifying that interrupts are enabled and no tracked
/// lock is held.
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
    operation()
}

/// Drop `value` through [`run_off_lock`].
#[inline]
pub fn drop_off_lock<T>(value: T) {
    run_off_lock(|| drop(value));
}
