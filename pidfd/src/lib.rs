//! `slopos-pidfd` — a process-exit fd (`FileKind::Pidfd`).
//!
//! `pidfd_open(pid)` returns an fd that becomes `POLLIN`-ready when the target
//! task exits, so a waiter can reach a child through `poll(2)` / SlopRing
//! `OP_POLL_ADD` instead of busy-polling `waitpid` with `WNOHANG`.

#![no_std]
#![forbid(unsafe_code)]

pub mod file_ops;

pub use file_ops::PIDFD_FILE_OPS;

use slopos_abi::Errno;
use slopos_fs::fileio::FdTable;

/// Returns the new fd (`>= 0`) or a negated errno:
/// - `-ESRCH` if no such task exists (e.g. already reaped),
/// - `-EPERM` if the target is not a child of the caller.
pub fn pidfd_open(table: FdTable, caller_task_id: u32, target_task_id: u32) -> i32 {
    let Some(task) = slopos_sched::task::task_find_by_id(target_task_id) else {
        return Errno::ESRCH.raw();
    };
    if task.parent_task_id() != caller_task_id {
        return Errno::EPERM.raw();
    }
    // No per-open kernel state, hence no backing to drop.
    slopos_fs::fileio_open_fd_with_ops(
        table,
        &file_ops::PIDFD_FILE_OPS,
        target_task_id as usize,
        None,
        slopos_fs::FdFlags::NONE,
    )
}
