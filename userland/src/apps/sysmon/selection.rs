//! Durable task identity, so a selection names a task rather than a position.
//!
//! The process table re-sorts on every refresh (once a second, and on any
//! CPU%/runtime change), so a row index designates a different task from one
//! frame to the next. Selecting by index meant the highlight sat still while
//! the tasks slid underneath it — and, worse, that the row-context menu and
//! the kill dialog acted on whoever had arrived at that coordinate.

use slopos_abi::syscall::types::UserTaskEntry;

/// Identity of one task instantiation.
///
/// `pid` alone is not an identity: ids recycle, so the same number can name a
/// different task after an exit. `started_ms` is the task's creation time,
/// which the kernel never rewrites, so the pair stays distinct across a
/// recycle — the case a name comparison misses when a respawned service
/// reuses both the number and the name.
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
