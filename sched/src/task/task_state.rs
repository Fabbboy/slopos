use core::ffi::c_int;

use super::task_accessors::task_borrow;
use super::task_table::task_find_by_id;
use super::{BlockReason, Task, TaskStatus};

pub fn task_set_state(task_id: u32, new_status: TaskStatus) -> c_int {
    let task = task_find_by_id(task_id);
    let Some(task_ref) = task_borrow(task) else {
        return -1;
    };
    if task_ref.status() == TaskStatus::Invalid {
        return -1;
    }

    transition_to_c_int(task_ref.try_transition_to(new_status))
}

#[inline]
fn transition_to_c_int(success: bool) -> c_int {
    if success { 0 } else { -1 }
}

fn apply_state_transition(task_ref: &Task, new_status: TaskStatus, reason: BlockReason) -> c_int {
    match new_status {
        TaskStatus::Ready => transition_to_c_int(task_ref.mark_ready()),
        TaskStatus::Running => transition_to_c_int(task_ref.mark_running()),
        TaskStatus::Blocked => transition_to_c_int(task_ref.block(reason)),
        TaskStatus::Terminated => transition_to_c_int(task_ref.terminate()),
        TaskStatus::Zombie => transition_to_c_int(task_ref.mark_zombie()),
        TaskStatus::Invalid => -1,
    }
}

pub fn task_set_state_with_reason(
    task_id: u32,
    new_status: TaskStatus,
    reason: BlockReason,
) -> c_int {
    let task = task_find_by_id(task_id);
    let Some(task_ref) = task_borrow(task) else {
        return -1;
    };
    if task_ref.status() == TaskStatus::Invalid {
        return -1;
    }

    apply_state_transition(task_ref, new_status, reason)
}

/// Atomically transition from `expected` to `target`.
///
/// Returns 0 on success, -1 if the current state does not match `expected`
/// or the transition is invalid.
pub fn task_try_transition_from(task_id: u32, expected: TaskStatus, target: TaskStatus) -> c_int {
    let task = task_find_by_id(task_id);
    let Some(task_ref) = task_borrow(task) else {
        return -1;
    };
    if task_ref.status() == TaskStatus::Invalid {
        return -1;
    }
    transition_to_c_int(task_ref.try_transition_from(expected, target))
}

/// Atomically transition from `expected` to `new_status`, setting block reason.
pub fn task_set_state_from_with_reason(
    task_id: u32,
    expected: TaskStatus,
    new_status: TaskStatus,
    reason: BlockReason,
) -> c_int {
    let task = task_find_by_id(task_id);
    let Some(task_ref) = task_borrow(task) else {
        return -1;
    };
    if task_ref.status() == TaskStatus::Invalid {
        return -1;
    }
    match new_status {
        TaskStatus::Blocked => transition_to_c_int(task_ref.block_from(expected, reason)),
        _ => transition_to_c_int(task_ref.try_transition_from(expected, new_status)),
    }
}
