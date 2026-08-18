//! Async process supervision: await a child's exit as a pidfd ring
//! completion, then reap it.

use slopos_abi::syscall::POLLIN;

use crate::sys::{pidfd, process};

/// A handle to a child task that can be awaited for exit.
pub struct Child {
    pid: u32,
}

impl Child {
    pub fn from_pid(pid: u32) -> Self {
        Self { pid }
    }

    /// Await the child's exit, then reap it. Resolves to the child's exit
    /// code (or a negated errno if it could not be opened/reaped).
    pub async fn wait(&self) -> i32 {
        let fd = pidfd::pidfd_open(self.pid);
        if fd < 0 {
            // Already reaped or not our child: a direct reap returns
            // immediately for a zombie.
            return process::waitpid(self.pid);
        }
        let _ = super::poll_add(fd, POLLIN).await;
        let _ = slopos_slibc::ffi::close(fd);
        process::waitpid(self.pid)
    }
}
