//! Durable task identity, so a selection names a task rather than a position.
//!
//! The process table re-sorts on every refresh, so a row index designates a
//! different task from one frame to the next.

use slopos_abi::syscall::types::UserTaskEntry;

/// Identity of one task instantiation.
///
/// Ids recycle, so `pid` alone is not an identity; `started_ms` is the creation
/// time the kernel never rewrites, keeping the pair distinct across a recycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskKey {
    pub pid: u32,
    pub started_ms: u64,
}

impl TaskKey {
    pub fn of(task: &UserTaskEntry) -> Self {
        Self {
            pid: task.task_id,
            started_ms: task.creation_time_ms,
        }
    }

    /// Whether `task` is the same instantiation this key names.
    pub fn matches(&self, task: &UserTaskEntry) -> bool {
        *self == Self::of(task)
    }
}

/// Position of `key` in the current sort order, or `None` if the task it
/// names is no longer in the table.
///
/// `order` holds indices into `tasks`, in display order.
pub fn row_of(key: TaskKey, tasks: &[UserTaskEntry], order: &[usize]) -> Option<usize> {
    order
        .iter()
        .position(|&idx| tasks.get(idx).is_some_and(|t| key.matches(t)))
}

/// The task shown at display row `row`.
pub fn key_at_row(tasks: &[UserTaskEntry], order: &[usize], row: usize) -> Option<TaskKey> {
    let idx = *order.get(row)?;
    tasks.get(idx).map(TaskKey::of)
}

/// Index into `tasks` for the task `key` names.
pub fn index_of(key: TaskKey, tasks: &[UserTaskEntry]) -> Option<usize> {
    tasks.iter().position(|t| key.matches(t))
}
