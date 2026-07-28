use core::ffi::c_int;

use super::task_table::task_find_by_id;
use super::{BlockReason, Task, TaskStatus};

pub fn task_set_state(task_id: u32, new_status: TaskStatus) -> c_int {
    let Some(task_ref) = task_find_by_id(task_id) else {
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
    let Some(task_ref) = task_find_by_id(task_id) else {
        return -1;
    };
    if task_ref.status() == TaskStatus::Invalid {
        return -1;
    }

    apply_state_transition(&task_ref, new_status, reason)
}

/// Atomically transition a task the caller already holds from `expected` to
/// `target`, reporting whether this caller won.
///
/// The id-keyed [`task_try_transition_from`] resolves the same task through the
/// registry; a caller that already has it — the wake path, which retries in a
/// loop — takes this instead and pays neither the cli-spinlock nor the scan.
pub fn task_transition_from(task: &Task, expected: TaskStatus, target: TaskStatus) -> bool {
    task.status() != TaskStatus::Invalid && task.try_transition_from(expected, target)
}

/// Atomically transition from `expected` to `target`.
///
/// Returns 0 on success, -1 if the task is gone, the current state does not
/// match `expected`, or the transition is invalid.
pub fn task_try_transition_from(task_id: u32, expected: TaskStatus, target: TaskStatus) -> c_int {
    let Some(task_ref) = task_find_by_id(task_id) else {
        return -1;
    };
    transition_to_c_int(task_transition_from(&task_ref, expected, target))
}

/// Atomically transition from `expected` to `new_status`, setting block reason.
pub fn task_set_state_from_with_reason(
    task_id: u32,
    expected: TaskStatus,
    new_status: TaskStatus,
    reason: BlockReason,
) -> c_int {
    let Some(task_ref) = task_find_by_id(task_id) else {
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
