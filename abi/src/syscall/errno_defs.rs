//! Linux-compatible errno values for syscall return registers, derived from
//! the typed [`crate::errno::Errno`] table — the single source of truth for
//! errno numbering.

use crate::errno::Errno;

pub const ERRNO_EPERM: u64 = Errno::EPERM.as_u64();
pub const ERRNO_ENOENT: u64 = Errno::ENOENT.as_u64();
pub const ERRNO_ESRCH: u64 = Errno::ESRCH.as_u64();
pub const ERRNO_EINTR: u64 = Errno::EINTR.as_u64();
pub const ERRNO_EIO: u64 = Errno::EIO.as_u64();
pub const ERRNO_ENXIO: u64 = Errno::ENXIO.as_u64();
pub const ERRNO_ECHILD: u64 = Errno::ECHILD.as_u64();
pub const ERRNO_EAGAIN: u64 = Errno::EAGAIN.as_u64();
pub const ERRNO_ENOMEM: u64 = Errno::ENOMEM.as_u64();
pub const ERRNO_EFAULT: u64 = Errno::EFAULT.as_u64();
pub const ERRNO_EBUSY: u64 = Errno::EBUSY.as_u64();
pub const ERRNO_ENOTDIR: u64 = Errno::ENOTDIR.as_u64();
pub const ERRNO_EINVAL: u64 = Errno::EINVAL.as_u64();
pub const ERRNO_ERANGE: u64 = Errno::ERANGE.as_u64();
pub const ERRNO_ENOTSOCK: u64 = Errno::ENOTSOCK.as_u64();
pub const ERRNO_EDESTADDRREQ: u64 = Errno::EDESTADDRREQ.as_u64();
pub const ERRNO_EPROTONOSUPPORT: u64 = Errno::EPROTONOSUPPORT.as_u64();
pub const ERRNO_EOPNOTSUPP: u64 = Errno::EOPNOTSUPP.as_u64();
pub const ERRNO_EAFNOSUPPORT: u64 = Errno::EAFNOSUPPORT.as_u64();
pub const ERRNO_EADDRINUSE: u64 = Errno::EADDRINUSE.as_u64();
pub const ERRNO_EADDRNOTAVAIL: u64 = Errno::EADDRNOTAVAIL.as_u64();
pub const ERRNO_ENETUNREACH: u64 = Errno::ENETUNREACH.as_u64();
pub const ERRNO_ECONNABORTED: u64 = Errno::ECONNABORTED.as_u64();
pub const ERRNO_ECONNRESET: u64 = Errno::ECONNRESET.as_u64();
pub const ERRNO_ENOBUFS: u64 = Errno::ENOBUFS.as_u64();
pub const ERRNO_EISCONN: u64 = Errno::EISCONN.as_u64();
pub const ERRNO_ENOTCONN: u64 = Errno::ENOTCONN.as_u64();
pub const ERRNO_ETIMEDOUT: u64 = Errno::ETIMEDOUT.as_u64();
pub const ERRNO_ECONNREFUSED: u64 = Errno::ECONNREFUSED.as_u64();
pub const ERRNO_EHOSTUNREACH: u64 = Errno::EHOSTUNREACH.as_u64();
pub const ERRNO_EINPROGRESS: u64 = Errno::EINPROGRESS.as_u64();
pub const ERRNO_EPIPE: u64 = Errno::EPIPE.as_u64();

/// Kernel-internal restartable-syscall sentinel. Must never reach userland;
/// the syscall return path converts it to `EINTR` or restarts the call.
pub const ERRNO_ERESTARTSYS: u64 = Errno::ERESTARTSYS.as_u64();
