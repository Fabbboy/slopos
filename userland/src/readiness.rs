//! Typed readiness signaling between parent and child processes.
//!
//! The parent creates a [`ReadinessGate`] before spawning the child.
//! The child inherits a [`ReadinessNotifier`] on a well-known FD and
//! calls [`signal_ready()`](ReadinessNotifier::signal_ready) when
//! initialization is complete. The parent's [`wait()`](ReadinessGate::wait)
//! blocks until the signal arrives.

use crate::syscall::fs;

const NOTIFIER_FD: i32 = 3;

/// Parent side: blocks until the child signals readiness.
pub struct ReadinessGate {
    read_fd: i32,
}

impl ReadinessGate {
    /// Create a gate and place the notifier end on fd 3 so the next
    /// spawned child inherits it.
    pub fn create() -> Option<Self> {
        let (read_end, write_end) = fs::pipe().ok()?;
        let read_fd = read_end.into_raw();
        let write_fd = write_end.into_raw();

        // Move the write end to the well-known FD. If the read end
        // happens to be on that slot, move it out of the way first.
        if read_fd == NOTIFIER_FD {
            let new_read = fs::dup(read_fd).ok()?.into_raw();
            let _ = slopos_slibc::ffi::close(read_fd);
            if write_fd != NOTIFIER_FD {
                let _ = fs::dup2(write_fd, NOTIFIER_FD);
                let _ = slopos_slibc::ffi::close(write_fd);
            }
            return Some(Self { read_fd: new_read });
        }

        if write_fd != NOTIFIER_FD {
            let _ = fs::dup2(write_fd, NOTIFIER_FD);
            let _ = slopos_slibc::ffi::close(write_fd);
        }

        Some(Self { read_fd })
    }

    /// Block until the child calls `ReadinessNotifier::signal_ready()`,
    /// or return immediately if the child died (broken pipe).
    pub fn wait(self) {
        let _ = slopos_slibc::ffi::close(NOTIFIER_FD);
        let mut buf = [0u8; 1];
        let _ =
            slopos_slibc::ffi::read(self.read_fd, buf.as_mut_ptr() as *mut core::ffi::c_void, 1);
        let _ = slopos_slibc::ffi::close(self.read_fd);
    }
}

/// Child side: signals the parent that initialization is complete.
pub struct ReadinessNotifier;

impl ReadinessNotifier {
    /// Acquire the notifier from the inherited FD.
    /// Returns `None` if the FD doesn't exist (standalone launch).
    pub fn acquire() -> Option<Self> {
        use slopos_abi::syscall::posix::F_GETFD;
        use slopos_slibc::pal::{Pal, Sys};
        Sys::fcntl(NOTIFIER_FD, F_GETFD as i32, 0).ok()?;
        Some(Self)
    }

    /// Signal readiness and consume the notifier.
    pub fn signal_ready(self) {
        let _ = slopos_slibc::ffi::write(NOTIFIER_FD, b"R".as_ptr() as *const core::ffi::c_void, 1);
        let _ = slopos_slibc::ffi::close(NOTIFIER_FD);
    }
}
