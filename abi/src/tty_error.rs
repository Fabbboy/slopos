//! TTY subsystem error types, shared across the kernel so service interfaces
//! can name `TtyError` directly instead of a lossy `Result -> i32` adapter.

/// Errors returned by TTY operations. Internal code matches variants directly;
/// [`TtyError::to_errno()`] is called only at the syscall return edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyError {
    InvalidIndex,
    NotAllocated,
    /// Caller is a background process — should receive SIGTTIN.
    BackgroundRead,
    /// Caller is a background process with TOSTOP — should receive SIGTTOU.
    BackgroundWrite,
    /// TTY is hung up — reads return EIO/EOF.
    HungUp,
    /// No data available and O_NONBLOCK is set — EAGAIN.
    WouldBlock,
    PermissionDenied,
    UnsupportedLineDiscipline,
    /// Caller belongs to a different session than the TTY's controlling session.
    CrossSessionDenied,
    SignalInterrupt,
    /// Background process in an orphaned process group tried to change
    /// terminal settings — returns EIO instead of SIGTTOU.
    OrphanedProcessGroup,
    InvalidArg,
    /// Device is in exclusive mode and already open.
    DeviceBusy,
    /// Kernel-internal ERESTARTSYS (-512): the syscall return path converts it
    /// to EINTR or restarts per SA_RESTART. MUST NEVER reach userland.
    Restart,
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
