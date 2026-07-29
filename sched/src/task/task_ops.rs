//! Re-export of the OSTD-owned task operations.
//!
//! The bodies live in `slopos_ostd::task::ops`; kernel callers spell each one
//! as `crate::task::task_ops::*`.
//!
//! The list is written out rather than globbed so the re-exported surface is
//! enumerable. Each operation that loses its last caller is then removed from
//! one named place, and the compiler reports the removal as an unresolved
//! import rather than letting a glob silently keep resolving.

pub use slopos_ostd::task::ops::{
    TASK_EXIT_CLEANUP_ACCOUNTED, TASK_EXIT_CLEANUP_RESOURCES, TASK_EXIT_CLEANUP_VM,
    child_exit_event, signal_pending_event, task_clone_from, task_default_signals_in_mask,
    task_has_deliverable_signal, task_install_idle_affinity, task_kernel_stack_seed_ret,
    task_reset_caught_handlers, task_reset_fpu_state, task_signal_post, task_signal_raise,
    task_waiter_count, task_wake_all_waiters,
};

use super::task_table::task_find_by_id;

/// Establish `parent_task_id` as the parent of `task_id`: sets the child's
/// parent id and parks one owning reference in the parent's children list (via
/// [`link_child`](super::link_child)). Returns 0 on success, -1 if either task
/// cannot be located. Both guards pin their task across the link.
pub fn task_set_parent(task_id: u32, parent_task_id: u32) -> core::ffi::c_int {
    let Some(child) = task_find_by_id(task_id) else {
        return -1;
    };
    let Some(parent) = task_find_by_id(parent_task_id) else {
        return -1;
    };
    let Some(child_nn) = core::ptr::NonNull::new(child.as_ptr()) else {
        return -1;
    };
    super::link_child(&parent, child_nn);
    0
}
