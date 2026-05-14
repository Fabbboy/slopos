//! Durable per-task exit value.
//!
//! Stored on each `Task` in an `AtomicCell<ExitInfo>`. `mark_task_terminated`
//! publishes via `try_set` *before* the wake fanout in
//! `release_task_dependents`; late waiters that race past the wake see the
//! published value on their next condition re-check and exit cleanly without
//! ever blocking. This is the durable source of truth for child-wait;
//! `TaskExitRecord` (in `mgr.exit_records`) remains a non-durable diagnostics
//! cache.

use slopos_abi::task::{TaskExitReason, TaskFaultReason};

#[derive(Clone, Debug)]
pub struct ExitInfo {
    pub exit_code: i32,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub signal: u8,
    pub exit_time_ms: u64,
}
