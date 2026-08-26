//! `FileKind::Pidfd` file operations.
//!
//! The open file's `handle: usize` *is* the target task id; a pidfd carries no
//! per-open kernel state. Task ids are monotonic and never recycled, so an
//! absent task is unambiguously dead.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::syscall::POLLIN;
use slopos_ostd::sync::PollWaiterRef;
use slopos_ostd::sync::event_bus::BUS;
use slopos_ostd::task::ops::child_exit_event;
use slopos_sched::task::task_find_by_id;

pub struct PidfdFileOps;

pub static PIDFD_FILE_OPS: PidfdFileOps = PidfdFileOps;

fn target_exited(task_id: u32) -> bool {
    task_find_by_id(task_id)
        .map(|task| task.is_exited())
        .unwrap_or(true)
}

impl FileOps for PidfdFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Pidfd
    }

    fn read(&self, _handle: usize, _buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        // Not readable, per Linux pidfd semantics; reap with waitpid.
        Errno::EINVAL.as_isize()
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn poll_wait(&self, handle: usize) -> bool {
        PollWaiterRef::current()
            .is_some_and(|w| BUS.subscribe_current(w, child_exit_event(handle as u32)))
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
