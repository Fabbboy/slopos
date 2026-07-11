//! Re-export of the OSTD-owned task accessor functions.
//!
//! The body relocated to `slopos_ostd::task::accessors`. Kernel callers
//! continue to spell each accessor as
//! `crate::task::task_accessors::*`.

pub use slopos_ostd::task::accessors::*;

use super::Task;
use super::task_table::{task_find_by_id, task_pointer_is_valid};

/// Validate the pointer through [`task_pointer_is_valid`]. Wraps the
/// downstream null/whitelist check so callers can short-circuit
/// before touching any field. Kernel-side because
/// `task_pointer_is_valid` reaches the kernel task registry.
#[inline]
pub fn task_validate(task: *const Task) -> Option<*const Task> {
    if task.is_null() {
        None
    } else if task_pointer_is_valid(task) {
        Some(task)
    } else {
        None
    }
}

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
    super::link_child(parent.as_ptr(), child.as_ptr());
    0
}
