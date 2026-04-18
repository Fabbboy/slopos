//! TTY subsystem error types.
//!
//! Canonical definitions for TTY errors shared across the kernel.  Moved here
//! from the drivers crate so that service interfaces in `kernel-services` can
//! reference `TtyError` directly in function pointer signatures, eliminating
//! the need for lossy `Result -> i32` adapter functions.

/// Errors returned by TTY operations.
///
/// # `to_errno()` boundary mapping
///
/// Each variant maps to a POSIX errno at the syscall boundary via
/// [`TtyError::to_errno()`].  Internal code matches on variants directly;
/// the syscall return path calls `to_errno()` at the very edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyError {
    /// TTY index is out of range (>= MAX_TTYS).
    InvalidIndex,
    /// TTY slot is not allocated (None).
    NotAllocated,
    /// Caller is a background process — should receive SIGTTIN.
    BackgroundRead,
    /// Caller is a background process with TOSTOP — should receive SIGTTOU.
    BackgroundWrite,
    /// TTY is hung up — reads return EIO/EOF.
    HungUp,
    /// No data available and O_NONBLOCK is set — EAGAIN.
    WouldBlock,
    /// Permission denied (e.g. different session for TIOCSPGRP).
    PermissionDenied,
    /// Unsupported line discipline ID.
    UnsupportedLineDiscipline,
    /// Caller belongs to a different session than the TTY's controlling
    /// session — hard denial.
    CrossSessionDenied,
    /// Operation was interrupted by a signal.
    SignalInterrupt,
    /// Background process in an orphaned process group tried to change
    /// terminal settings — returns EIO instead of SIGTTOU.
    OrphanedProcessGroup,
    /// Invalid argument.
    InvalidArg,
    /// Device is in exclusive mode and already open.
    DeviceBusy,
    /// Blocking syscall was interrupted by a signal and
    /// may be transparently restarted.  Maps to the kernel-internal
    /// ERESTARTSYS (-512) — the syscall return path converts this to EINTR
    /// or restarts depending on SA_RESTART.  MUST NEVER reach userland.
    Restart,
    /// Allocator returned out of memory during a TTY operation.
    OutOfMemory,
}

impl crate::KernelErrno for TtyError {
    #[inline]
    fn to_errno(&self) -> i32 {
        use crate::syscall::*;
        match self {
            Self::InvalidIndex => ERRNO_EINVAL as i32,
            Self::NotAllocated => ERRNO_ENXIO as i32,
            Self::BackgroundRead => ERRNO_EIO as i32,
            Self::BackgroundWrite => ERRNO_EIO as i32,
            Self::HungUp => ERRNO_EIO as i32,
            Self::WouldBlock => ERRNO_EAGAIN as i32,
            Self::PermissionDenied => ERRNO_EPERM as i32,
            Self::UnsupportedLineDiscipline => ERRNO_EINVAL as i32,
            Self::CrossSessionDenied => ERRNO_EIO as i32,
            Self::SignalInterrupt => ERRNO_EINTR as i32,
            Self::OrphanedProcessGroup => ERRNO_EIO as i32,
            Self::InvalidArg => ERRNO_EINVAL as i32,
            Self::DeviceBusy => ERRNO_EBUSY as i32,
            Self::Restart => ERRNO_ERESTARTSYS as i32,
            Self::OutOfMemory => ERRNO_ENOMEM as i32,
        }
    }
}
