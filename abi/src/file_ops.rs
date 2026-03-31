//! File-operations vtable for polymorphic file descriptors.

use crate::fs::UserFsStat;
use crate::io::{IoBufRead, IoBufWrite};

/// Result of a fused poll operation: readiness bits + registration status.
///
/// Returned by [`FileOps::poll_fused`].  The fused poll pattern (modeled on
/// Linux's `->poll()` callback) combines wait-queue registration and readiness
/// checking in a single call, eliminating the race window between separate
/// `poll_wait` and `poll_events` calls.
#[derive(Debug, Clone, Copy)]
pub struct FusedPollResult {
    /// POLL* bitmask of currently ready events.
    pub revents: u16,
    /// `true` if the caller was registered on a wait queue for wakeup.
    pub registered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileKind {
    Regular = 0,
    Socket = 1,
    PipeRead = 2,
    PipeWrite = 3,
    Tty = 4,
}

/// Per-resource-type operations for open file descriptions.
///
/// Subsystems provide **static** (zero-sized) implementations. Per-open
/// state is identified by the opaque `handle` passed to every method.
///
pub trait FileOps: Send + Sync {
    fn kind(&self) -> FileKind;

    /// Read data from this file into `buf`.
    ///
    /// Returns bytes read on success, or a negative errno.
    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, offset: u64, flags: u32) -> isize;

    /// Write data from `buf` into this file.
    ///
    /// Returns bytes written on success, or a negative errno.
    fn write(&self, handle: usize, buf: &dyn IoBufRead, offset: u64, flags: u32) -> isize;

    /// Called exactly once when refcount reaches zero.
    fn release(&self, handle: usize);

    /// Returns `Some(new_handle)` on success. Default: same handle (no-op dup).
    fn dup(&self, handle: usize) -> Option<usize> {
        Some(handle)
    }

    /// Fused poll: register waiter + check readiness in one call.
    ///
    /// Modeled on Linux's `->poll()` file operation.  Implementations
    /// should register the current task on the appropriate wait queue
    /// AND check readiness under the same subsystem lock, eliminating
    /// the race window that exists when these are separate calls.
    ///
    /// The default delegates to the legacy `poll_wait` + `poll_events`
    /// methods for backward compatibility during incremental migration.
    fn poll_fused(&self, handle: usize, events: u16) -> FusedPollResult {
        let registered = if events != 0 {
            self.poll_wait(handle)
        } else {
            false
        };
        let revents = self.poll_events(handle, events);
        FusedPollResult {
            revents,
            registered,
        }
    }

    /// Returns POLL* bitmask of ready events.
    ///
    /// **Legacy** — prefer `poll_fused` for new code.
    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let _ = (handle, events);
        0
    }

    /// **Legacy** — prefer `poll_fused` for new code.
    fn poll_wait(&self, handle: usize) -> bool {
        let _ = handle;
        false
    }

    /// **Legacy** — prefer `poll_fused` for new code.
    fn poll_unwait(&self, handle: usize) {
        let _ = handle;
    }

    fn stat(&self, handle: usize, out: &mut UserFsStat) -> i32 {
        let _ = (handle, out);
        -1
    }

    /// Notify subsystem that status flags (`O_NONBLOCK` etc.) changed.
    fn set_status_flags(&self, handle: usize, flags: u32) -> i32 {
        let _ = (handle, flags);
        0
    }

    fn ioctl(&self, handle: usize, cmd: u64, arg: u64) -> isize {
        let _ = (handle, cmd, arg);
        -1
    }

    fn seekable(&self) -> bool {
        false
    }

    fn size(&self, handle: usize) -> Option<u64> {
        let _ = handle;
        None
    }
}
