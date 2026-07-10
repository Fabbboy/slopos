//! `slopos-pidfd` — a process-exit fd (`FileKind::Pidfd`).
//!
//! `pidfd_open(pid)` returns an fd that becomes `POLLIN`-ready when the
//! target task exits. It lets a waiter (e.g. the shell) wait for a child
//! via `poll(2)` / SlopRing `OP_POLL_ADD` instead of busy-polling
//! `waitpid` with `WNOHANG` — closing the one userland polling loop the
//! ring alone could not (process reaping has no fd to submit against).
//!
//! Strictly synchronous, no `unsafe`: the heavy lifting is the existing
//! task registry + event bus. See [`file_ops`] for the readiness contract.

#![no_std]
#![forbid(unsafe_code)]

pub mod file_ops;

pub use file_ops::PIDFD_FILE_OPS;

use slopos_abi::Errno;

/// Open a pidfd for `target_task_id` on behalf of caller task `caller_task_id`
/// in process `process_id`.
///
/// Returns the new fd (`>= 0`) or a negated errno:
/// - `-ESRCH` if no such task exists (e.g. already reaped),
/// - `-EPERM` if the target is not a child of the caller.
///
/// On success the fd is `FileKind::Pidfd` with the target task id as its
/// opaque handle (see [`file_ops`]).
pub fn pidfd_open(process_id: u32, caller_task_id: u32, target_task_id: u32) -> i32 {
    let Some(task) = slopos_sched::task::task_find_by_id(target_task_id) else {
        return Errno::ESRCH.raw();
    };
    // Restrict to children of the caller: a pidfd is for reaping your own
    // children, mirroring how `waitpid` is scoped.
    if slopos_ostd::task::accessors::task_parent_task_id(task.as_ptr()) != Some(caller_task_id) {
        return Errno::EPERM.raw();
    }
    // A pidfd carries no per-open kernel state, so it has no backing to drop.
    slopos_fs::fileio_open_fd_with_ops(
        process_id,
        &file_ops::PIDFD_FILE_OPS,
        target_task_id as usize,
        None,
    )
}
