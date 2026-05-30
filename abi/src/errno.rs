//! Type-safe POSIX errno for kernel-internal use.
//!
//! All errno constants, their [`Debug`] representation, and
//! human-readable names are generated from a single invocation of
//! [`define_errnos!`] — adding a new code requires exactly one line.
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
}

macro_rules! define_errnos {
    ( $( $(#[$meta:meta])* $name:ident = $val:literal; )* ) => {
        impl Errno {
            $( $(#[$meta])* pub const $name: Self = Self::new(-$val); )*
        }

        impl Errno {
            /// Return the symbolic name of this errno (e.g. `"EPERM"`).
            pub const fn name(self) -> &'static str {
                match self.raw() {
                    $( -$val => stringify!($name), )*
                    _ => "UNKNOWN",
                }
            }
        }

        impl fmt::Debug for Errno {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let n = self.name();
                if n == "UNKNOWN" {
                    write!(f, "Errno({})", self.raw())
                } else {
                    write!(f, "Errno::{n}")
                }
            }
        }
    };
}

// ── Single source of truth for all errno constants ─────────────────────
//
// Values follow the Linux/x86-64 numbering from
// include/uapi/asm-generic/errno-base.h and errno.h.
define_errnos! {
    EPERM           =   1;
    ENOENT          =   2;
    ESRCH           =   3;
    EINTR           =   4;
    EIO             =   5;
    ENXIO           =   6;
    E2BIG           =   7;
    ENOEXEC         =   8;
    EBADF           =   9;
    ECHILD          =  10;
    EAGAIN          =  11;
    ENOMEM          =  12;
    EACCES          =  13;
    EFAULT          =  14;
    EBUSY           =  16;
    EEXIST          =  17;
    ENODEV          =  19;
    ENOTDIR         =  20;
    EISDIR          =  21;
    EINVAL          =  22;
    ENFILE          =  23;
    EMFILE          =  24;
    ENOSPC          =  28;
    ESPIPE          =  29;
    EPIPE           =  32;
    ERANGE          =  34;
    ENAMETOOLONG    =  36;
    ENOSYS          =  38;
    ENOTEMPTY       =  39;
    ETIME           =  62;
    ENOTSOCK        =  88;
    EDESTADDRREQ    =  89;
    EPROTONOSUPPORT =  93;
    EOPNOTSUPP      =  95;
    EAFNOSUPPORT    =  97;
    EADDRINUSE      =  98;
    EADDRNOTAVAIL   =  99;
    ENETUNREACH     = 101;
    ECONNABORTED    = 103;
    ECONNRESET      = 104;
    ENOBUFS         = 105;
    EISCONN         = 106;
    ENOTCONN        = 107;
    ETIMEDOUT       = 110;
    ECONNREFUSED    = 111;
    EHOSTUNREACH    = 113;
    EALREADY        = 114;
    EINPROGRESS     = 115;
    ECANCELED       = 125;
    /// Kernel-internal: restartable syscall.  **Must never reach userland.**
    ERESTARTSYS     = 512;
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
