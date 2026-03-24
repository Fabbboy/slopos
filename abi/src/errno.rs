//! Type-safe POSIX errno for kernel-internal use.
//!
//! Modeled after the Linux Rust bindings (`kernel::error::Error`).
//! [`Errno`] wraps a [`NonZeroI32`] that is always a valid negative
//! POSIX error code, verified at compile time for constants and at
//! runtime for dynamic values.
//!
//! ## Usage
//!
//! ```ignore
//! use slopos_abi::errno::Errno;
//!
//! fn do_io() -> Result<usize, Errno> {
//!     Err(Errno::EFAULT)
//! }
//! ```

use core::fmt;
use core::num::NonZeroI32;

/// A valid, non-zero, negative POSIX errno value.
///
/// The inner [`NonZeroI32`] is always in the range `[-4095, -1]`.
/// This is enforced by [`Errno::new`] (compile-time for constants)
/// and [`Errno::from_raw`] (runtime for dynamic values).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Errno(NonZeroI32);

impl Errno {
    /// Create an `Errno` from a negative error code.
    ///
    /// # Panics
    ///
    /// Panics (at compile time for `const` usage) if `errno` is not
    /// in `[-4095, -1]`.
    pub const fn new(errno: i32) -> Self {
        assert!(errno < 0, "errno must be negative");
        assert!(errno >= -4095, "errno out of range");
        match NonZeroI32::new(errno) {
            Some(v) => Self(v),
            None => panic!("errno must be non-zero"),
        }
    }

    /// Try to create an `Errno` from a raw value.
    ///
    /// Returns `None` if `errno` is not in `[-4095, -1]`.
    pub const fn from_raw(errno: i32) -> Option<Self> {
        if errno >= -4095 && errno < 0 {
            match NonZeroI32::new(errno) {
                Some(v) => Some(Self(v)),
                None => None,
            }
        } else {
            None
        }
    }

    /// Raw negative errno value (always in `[-4095, -1]`).
    #[inline]
    pub const fn raw(self) -> i32 {
        self.0.get()
    }

    /// Convert to `isize` for syscall return paths.
    #[inline]
    pub const fn as_isize(self) -> isize {
        self.0.get() as isize
    }

    /// Convert to `u64` for syscall register returns (sign-extended).
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0.get() as i64 as u64
    }

    // ── POSIX errno constants ──────────────────────────────────────────

    pub const EPERM: Self = Self::new(-1);
    pub const ENOENT: Self = Self::new(-2);
    pub const ESRCH: Self = Self::new(-3);
    pub const EINTR: Self = Self::new(-4);
    pub const EIO: Self = Self::new(-5);
    pub const ENXIO: Self = Self::new(-6);
    pub const ENOEXEC: Self = Self::new(-8);
    pub const EBADF: Self = Self::new(-9);
    pub const ECHILD: Self = Self::new(-10);
    pub const EAGAIN: Self = Self::new(-11);
    pub const ENOMEM: Self = Self::new(-12);
    pub const EACCES: Self = Self::new(-13);
    pub const EFAULT: Self = Self::new(-14);
    pub const EBUSY: Self = Self::new(-16);
    pub const EEXIST: Self = Self::new(-17);
    pub const ENOTDIR: Self = Self::new(-20);
    pub const EISDIR: Self = Self::new(-21);
    pub const EINVAL: Self = Self::new(-22);
    pub const EMFILE: Self = Self::new(-24);
    pub const ENOSPC: Self = Self::new(-28);
    pub const EPIPE: Self = Self::new(-32);
    pub const ERANGE: Self = Self::new(-34);
    pub const ENOSYS: Self = Self::new(-38);
    pub const ENOTEMPTY: Self = Self::new(-39);
    pub const ENOTSOCK: Self = Self::new(-88);
    pub const EDESTADDRREQ: Self = Self::new(-89);
    pub const EPROTONOSUPPORT: Self = Self::new(-93);
    pub const EOPNOTSUPP: Self = Self::new(-95);
    pub const EAFNOSUPPORT: Self = Self::new(-97);
    pub const EADDRINUSE: Self = Self::new(-98);
    pub const EADDRNOTAVAIL: Self = Self::new(-99);
    pub const ENETUNREACH: Self = Self::new(-101);
    pub const ECONNABORTED: Self = Self::new(-103);
    pub const ECONNRESET: Self = Self::new(-104);
    pub const ENOBUFS: Self = Self::new(-105);
    pub const EISCONN: Self = Self::new(-106);
    pub const ENOTCONN: Self = Self::new(-107);
    pub const ETIMEDOUT: Self = Self::new(-110);
    pub const ECONNREFUSED: Self = Self::new(-111);
    pub const EHOSTUNREACH: Self = Self::new(-113);
    pub const EINPROGRESS: Self = Self::new(-115);

    /// Kernel-internal: restartable syscall.  **Must never reach userland.**
    pub const ERESTARTSYS: Self = Self::new(-512);
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.raw() {
            -1 => "EPERM",
            -2 => "ENOENT",
            -3 => "ESRCH",
            -4 => "EINTR",
            -5 => "EIO",
            -6 => "ENXIO",
            -9 => "EBADF",
            -11 => "EAGAIN",
            -12 => "ENOMEM",
            -13 => "EACCES",
            -14 => "EFAULT",
            -16 => "EBUSY",
            -22 => "EINVAL",
            -38 => "ENOSYS",
            _ => return write!(f, "Errno({})", self.raw()),
        };
        write!(f, "Errno::{name}")
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl From<Errno> for i32 {
    #[inline]
    fn from(e: Errno) -> i32 {
        e.raw()
    }
}

impl From<Errno> for isize {
    #[inline]
    fn from(e: Errno) -> isize {
        e.as_isize()
    }
}
