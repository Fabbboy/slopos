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
    ( $( $(#[$meta:meta])* $name:ident = $val:literal, $desc:literal; )* ) => {
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

            /// Return the sentence-case description of this errno (e.g.
            /// `"Operation not permitted"`), for text a user reads.
            ///
            /// Wording follows the POSIX description of each code, so it
            /// states what the *code* means and never what a particular
            /// caller was doing when it got one.
            pub const fn description(self) -> &'static str {
                match self.raw() {
                    $( -$val => $desc, )*
                    _ => "Unknown error",
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
    EPERM           =   1, "Operation not permitted";
    ENOENT          =   2, "No such file or directory";
    ESRCH           =   3, "No such process";
    EINTR           =   4, "Interrupted system call";
    EIO             =   5, "Input/output error";
    ENXIO           =   6, "No such device or address";
    E2BIG           =   7, "Argument list too long";
    ENOEXEC         =   8, "Executable format error";
    EBADF           =   9, "Bad file descriptor";
    ECHILD          =  10, "No child processes";
    EAGAIN          =  11, "Resource temporarily unavailable";
    ENOMEM          =  12, "Cannot allocate memory";
    EACCES          =  13, "Permission denied";
    EFAULT          =  14, "Bad address";
    EBUSY           =  16, "Device or resource busy";
    EEXIST          =  17, "File exists";
    ENODEV          =  19, "No such device";
    ENOTDIR         =  20, "Not a directory";
    EISDIR          =  21, "Is a directory";
    EINVAL          =  22, "Invalid argument";
    ENFILE          =  23, "Too many open files in system";
    EMFILE          =  24, "Too many open files";
    ENOSPC          =  28, "No space left on device";
    ESPIPE          =  29, "Illegal seek";
    EPIPE           =  32, "Broken pipe";
    ERANGE          =  34, "Numerical result out of range";
    ENAMETOOLONG    =  36, "File name too long";
    ENOSYS          =  38, "Function not implemented";
    ENOTEMPTY       =  39, "Directory not empty";
    ETIME           =  62, "Timer expired";
    ENOTSOCK        =  88, "Not a socket";
    EDESTADDRREQ    =  89, "Destination address required";
    EPROTONOSUPPORT =  93, "Protocol not supported";
    EOPNOTSUPP      =  95, "Operation not supported";
    EAFNOSUPPORT    =  97, "Address family not supported";
    EADDRINUSE      =  98, "Address already in use";
    EADDRNOTAVAIL   =  99, "Cannot assign requested address";
    ENETUNREACH     = 101, "Network is unreachable";
    ECONNABORTED    = 103, "Connection aborted";
    ECONNRESET      = 104, "Connection reset by peer";
    ENOBUFS         = 105, "No buffer space available";
    EISCONN         = 106, "Socket is already connected";
    ENOTCONN        = 107, "Socket is not connected";
    ETIMEDOUT       = 110, "Connection timed out";
    ECONNREFUSED    = 111, "Connection refused";
    EHOSTUNREACH    = 113, "No route to host";
    EALREADY        = 114, "Operation already in progress";
    EINPROGRESS     = 115, "Operation now in progress";
    ECANCELED       = 125, "Operation canceled";
    /// Kernel-internal: restartable syscall.  **Must never reach userland.**
    ERESTARTSYS     = 512, "Restartable system call";
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
