//! File-operations vtable for polymorphic file descriptors.

use crate::fs::UserFsStat;

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
pub trait FileOps: Send + Sync {
    fn kind(&self) -> FileKind;

    /// Returns bytes read on success, or a negative errno.
    fn read(&self, handle: usize, buf: &mut [u8], offset: u64, flags: u32) -> isize;

    /// Returns bytes written on success, or a negative errno.
    fn write(&self, handle: usize, buf: &[u8], offset: u64, flags: u32) -> isize;

    /// Called exactly once when refcount reaches zero.
    fn release(&self, handle: usize);

    /// Returns `Some(new_handle)` on success. Default: same handle (no-op dup).
    fn dup(&self, handle: usize) -> Option<usize> {
        Some(handle)
    }

    /// Returns POLL* bitmask of ready events.
    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let _ = (handle, events);
        0
    }

    fn poll_wait(&self, handle: usize) -> bool {
        let _ = handle;
        false
    }

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
