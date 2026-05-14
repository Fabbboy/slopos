//! Re-export of the OSTD-owned task accessor functions.
//!
//! The body relocated to `slopos_ostd::task::accessors`. Kernel callers
//! continue to spell each accessor as
//! `crate::scheduler::task::task_accessors::*`.

pub use slopos_ostd::task::accessors::*;

use super::Task;
use super::task_table::{task_find_by_id, task_pointer_is_valid};

/// Validate the pointer through [`task_pointer_is_valid`]. Wraps the
/// downstream null/whitelist check so callers can short-circuit
/// before touching any field. Kernel-side because
/// `task_pointer_is_valid` reaches the kernel's task pool.
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

/// Override the `parent_task_id` for a slot by task_id. Returns 0 on
/// success, -1 if the slot cannot be located. Kernel-side because
/// `task_find_by_id` reaches the kernel's task pool. The unsafe
/// field-write lives inside OSTD via
/// [`task_set_parent_task_id`](slopos_ostd::task::accessors::task_set_parent_task_id).
pub fn task_set_parent(task_id: u32, parent_task_id: u32) -> core::ffi::c_int {
    let task = task_find_by_id(task_id);
    if task.is_null() {
        return -1;
    }
    slopos_ostd::task::accessors::task_set_parent_task_id(task, parent_task_id);
    0
}
