//! Async process supervision: await a child's exit as a ring completion via
//! a pidfd, then reap it — closing the one polling loop the ring alone could
//! not (process reaping has no fd to submit against without pidfd).

use slopos_abi::syscall::POLLIN;

use crate::sys::{pidfd, process};

/// A handle to a child task that can be awaited for exit.
pub struct Child {
    pid: u32,
}

impl Child {
    /// Wrap a child task id (e.g. the return value of `fork`).
    pub fn from_pid(pid: u32) -> Self {
        Self { pid }
    }

    /// Await the child's exit, then reap it. Resolves to the child's exit
    /// code (or a negated errno if it could not be opened/reaped).
    pub async fn wait(&self) -> i32 {
        let fd = pidfd::pidfd_open(self.pid);
        if fd < 0 {
            // Already reaped or not a child of ours — fall back to a direct
            // (blocking) reap, which returns immediately for a zombie.
            return process::waitpid(self.pid);
        }
        // OP_POLL_ADD blocks (deferred) until the pidfd is POLLIN-ready, i.e.
        // the child has exited; the harvest is woken by the ChildExit event.
        let _ = super::poll_add(fd, POLLIN).await;
        let _ = slopos_slibc::ffi::close(fd);
        process::waitpid(self.pid)
    }
}
