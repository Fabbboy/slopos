use core::fmt;

/// Syscall error with errno-compatible representation.
#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(transparent)]
pub struct SyscallError(i32);

impl SyscallError {
    pub const EPERM: Self = Self(1);
    pub const ENOENT: Self = Self(2);
    pub const ESRCH: Self = Self(3);
    pub const EINTR: Self = Self(4);
    pub const EIO: Self = Self(5);
    pub const ENXIO: Self = Self(6);
    pub const E2BIG: Self = Self(7);
    pub const ENOEXEC: Self = Self(8);
    pub const EBADF: Self = Self(9);
    pub const ECHILD: Self = Self(10);
    pub const EAGAIN: Self = Self(11);
    pub const ENOMEM: Self = Self(12);
    pub const EACCES: Self = Self(13);
    pub const EFAULT: Self = Self(14);
    pub const EBUSY: Self = Self(16);
    pub const EEXIST: Self = Self(17);
    pub const EXDEV: Self = Self(18);
    pub const ENODEV: Self = Self(19);
    pub const ENOTDIR: Self = Self(20);
    pub const EISDIR: Self = Self(21);
    pub const EINVAL: Self = Self(22);
    pub const EMFILE: Self = Self(24);
    pub const EFBIG: Self = Self(27);
    pub const ENOSPC: Self = Self(28);
    pub const ESPIPE: Self = Self(29);
    pub const EROFS: Self = Self(30);
    pub const EPIPE: Self = Self(32);
    pub const ENOSYS: Self = Self(38);
    pub const ENETUNREACH: Self = Self(101);
    pub const ETIMEDOUT: Self = Self(110);
    pub const ECONNREFUSED: Self = Self(111);
    pub const EHOSTUNREACH: Self = Self(113);

    #[inline]
    pub const fn from_errno(errno: i32) -> Self {
        Self(errno)
    }

    #[inline]
    pub const fn errno(self) -> i32 {
        self.0
    }

    pub const fn as_str(self) -> &'static str {
        match self.0 {
            1 => "Operation not permitted",
            2 => "No such file or directory",
            3 => "No such process",
            4 => "Interrupted system call",
            5 => "I/O error",
            6 => "No such device or address",
            7 => "Argument list too long",
            8 => "Exec format error",
            9 => "Bad file descriptor",
            10 => "No child processes",
            11 => "Resource temporarily unavailable",
            12 => "Out of memory",
            13 => "Permission denied",
            14 => "Bad address",
            16 => "Device or resource busy",
            17 => "File exists",
            18 => "Cross-device link",
            19 => "No such device",
            20 => "Not a directory",
            21 => "Is a directory",
            22 => "Invalid argument",
            24 => "Too many open files",
            27 => "File too large",
            28 => "No space left on device",
            29 => "Illegal seek",
            30 => "Read-only file system",
            32 => "Broken pipe",
            38 => "Function not implemented",
            101 => "Network is unreachable",
            110 => "Connection timed out",
            111 => "Connection refused",
            113 => "No route to host",
            _ => "Unknown error",
        }
    }
}

impl fmt::Debug for SyscallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SyscallError({}: {})", self.0, self.as_str())
    }
}

impl fmt::Display for SyscallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl From<crate::errno::Errno> for SyscallError {
    #[inline]
    fn from(e: crate::errno::Errno) -> Self {
        SyscallError::from_errno(e.raw())
    }
}

pub type SyscallResult<T> = Result<T, SyscallError>;

/// Convert raw syscall return value to Result.
/// Negative values in [-4095, -1] indicate errors (negated errno).
#[inline]
pub fn demux(value: u64) -> SyscallResult<u64> {
    let signed = value as i64;
    if signed >= -4095 && signed < 0 {
        Err(SyscallError((-signed) as i32))
    } else {
        Ok(value)
    }
}

#[inline]
pub fn mux(result: SyscallResult<u64>) -> u64 {
    match result {
        Ok(v) => v,
        Err(e) => (-e.0 as i64) as u64,
    }
}
