//! `FileKind::Pidfd` file operations.
//!
//! A pidfd is an open file whose `handle: usize` *is* the target task id.
//! It carries no per-open kernel state — it is a thin, pollable view of a
//! task's exit status:
//!
//! - `poll_events` returns `POLLIN` once the target has exited (or is gone,
//!   i.e. already reaped — which counts as exited).
//! - `poll_wait` subscribes the calling task to the target's
//!   `KernelEvent::ChildExit` — the *same* event the kernel publishes from
//!   the task-exit path (`task_wake_all_waiters`) and that `waitpid` waits
//!   on. No new kernel event, no new wakeup mechanism.
//! - `read`/`write` are meaningless (`-EINVAL`); the exit status is reaped
//!   with the existing `waitpid` syscall once the fd signals.
//!
//! Lifetime: the kernel's task `KBox`es live for the kernel's lifetime
//! (see `task_find_by_id`), so dereferencing the looked-up pointer is
//! always sound — there is no use-after-free. The only residual ambiguity
//! is task-id *recycling* (the slot's identity changing after a reap),
//! which is the same benign staleness `task_wait_for`'s readiness check
//! tolerates; for a pidfd's short lifetime (open → poll → reap) it is not
//! a concern.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::syscall::POLLIN;
use slopos_ostd::sync::event_bus::BUS;
use slopos_ostd::task::accessors::child_exit_event;
use slopos_sched::task::{task_find_by_id, task_is_exited};

pub struct PidfdFileOps;

pub static PIDFD_FILE_OPS: PidfdFileOps = PidfdFileOps;

/// `true` once the target task (by id) has exited, or is gone entirely.
fn target_exited(task_id: u32) -> bool {
    let task = task_find_by_id(task_id);
    // A null lookup means the task was already reaped — treat as exited.
    task.is_null() || task_is_exited(task)
}

impl FileOps for PidfdFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Pidfd
    }

    fn read(&self, _handle: usize, _buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        // A pidfd is not readable (Linux semantics); reap with waitpid.
        Errno::EINVAL.as_isize()
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn poll_wait(&self, handle: usize) -> bool {
        // Register the calling task on the target's child-exit queue — the
        // exact register-then-recheck shape poll(2) / OP_POLL_ADD expect
        // (the default `poll_fused` calls this then `poll_events`).
        BUS.subscribe_current(child_exit_event(handle as u32))
    }

    fn poll_unwait(&self, handle: usize) {
        BUS.unsubscribe_current(child_exit_event(handle as u32));
    }

    fn poll_events(&self, handle: usize, _events: u16) -> u16 {
        if target_exited(handle as u32) {
            POLLIN
        } else {
            0
        }
    }
}
