// =============================================================================
// Errno constants (Linux-compatible negative values)
// =============================================================================

pub const ERRNO_EINVAL: u64 = (-22i64) as u64;
pub const ERRNO_ENOMEM: u64 = (-12i64) as u64;
pub const ERRNO_EAGAIN: u64 = (-11i64) as u64;
pub const ERRNO_ESRCH: u64 = (-3i64) as u64;
pub const ERRNO_EFAULT: u64 = (-14i64) as u64;
pub const ERRNO_ENOENT: u64 = (-2i64) as u64;
pub const ERRNO_ENOTDIR: u64 = (-20i64) as u64;
pub const ERRNO_ERANGE: u64 = (-34i64) as u64;
pub const ERRNO_ETIMEDOUT: u64 = (-110i64) as u64;
pub const ERRNO_EADDRINUSE: u64 = (-98i64) as u64;
pub const ERRNO_ECONNREFUSED: u64 = (-111i64) as u64;
pub const ERRNO_ENOTCONN: u64 = (-107i64) as u64;
pub const ERRNO_EISCONN: u64 = (-106i64) as u64;
pub const ERRNO_ENOTSOCK: u64 = (-88i64) as u64;
pub const ERRNO_EAFNOSUPPORT: u64 = (-97i64) as u64;
pub const ERRNO_EPROTONOSUPPORT: u64 = (-93i64) as u64;
pub const ERRNO_EDESTADDRREQ: u64 = (-89i64) as u64;
pub const ERRNO_ENETUNREACH: u64 = (-101i64) as u64;
pub const ERRNO_EHOSTUNREACH: u64 = (-113i64) as u64;
pub const ERRNO_ECONNRESET: u64 = (-104i64) as u64;
pub const ERRNO_ECONNABORTED: u64 = (-103i64) as u64;
pub const ERRNO_EADDRNOTAVAIL: u64 = (-99i64) as u64;
pub const ERRNO_ENOBUFS: u64 = (-105i64) as u64;
pub const ERRNO_EINPROGRESS: u64 = (-115i64) as u64;
pub const ERRNO_EOPNOTSUPP: u64 = (-95i64) as u64;
pub const ERRNO_EPIPE: u64 = (-32i64) as u64;
pub const ERRNO_EPERM: u64 = (-1i64) as u64;
pub const ERRNO_EINTR: u64 = (-4i64) as u64;
pub const ERRNO_EIO: u64 = (-5i64) as u64;
pub const ERRNO_ENXIO: u64 = (-6i64) as u64;
pub const ERRNO_EBUSY: u64 = (-16i64) as u64;

/// Internal-only error code for restartable syscalls.  MUST NEVER reach
/// userland — the syscall return path converts it to `ERRNO_EINTR` or
/// transparently restarts the syscall based on `SA_RESTART`.
pub const ERRNO_ERESTARTSYS: u64 = (-512i64) as u64;
