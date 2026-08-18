//! Durable per-task exit value, and the source of truth for child-wait.
//!
//! `mark_task_terminated` publishes here *before* the wake fanout, so a waiter
//! that races past the wake still sees it on its next condition re-check.
//! `TaskExitRecord` is a non-durable diagnostics cache, not this.

use slopos_abi::task::{TaskExitReason, TaskFaultReason};

#[derive(Clone, Debug)]
pub struct ExitInfo {
    pub exit_code: i32,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub signal: u8,
    pub exit_time_ms: u64,
}
