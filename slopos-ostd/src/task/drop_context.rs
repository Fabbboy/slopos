//! Context checks and helpers for destructors that may return memory to the
//! kernel allocator.

use crate::cpu::x86_64::interrupts;
use crate::sync::held_lock_count;

/// Assert that a task destructor is running in a context where allocator and
/// synchronous TLB-reclaim work may safely execute.
#[inline]
pub fn assert_task_drop_context() {
    debug_assert!(
        interrupts::are_interrupts_enabled(),
        "Task dropped with interrupts disabled"
    );
    debug_assert_eq!(
        held_lock_count(),
        0,
        "Task dropped while the current CPU holds a tracked lock"
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
    debug_assert_eq!(
        held_lock_count(),
        0,
        "deferred drop attempted while the current CPU holds a tracked lock"
    );
    operation()
}

/// Drop `value` through [`run_off_lock`].
#[inline]
pub fn drop_off_lock<T>(value: T) {
    run_off_lock(|| drop(value));
}
