//! POSIX errno — per-thread via TCB when TLS is initialized, static fallback
//! during early boot.

use core::fmt;

/// POSIX errno value — a newtype over the raw error number.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct Errno(pub i32);

impl Errno {
    /// Extract the raw errno integer.
    #[inline]
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// Returns `true` when the value represents success (zero).
    #[inline]
    pub const fn is_ok(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Errno({})", self.0)
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "errno {}", self.0)
    }
}

impl From<crate::error::SyscallError> for Errno {
    #[inline]
    fn from(e: crate::error::SyscallError) -> Self {
        Errno(e.errno())
    }
}

pub const EPERM: Errno = Errno(1);
pub const ENOENT: Errno = Errno(2);
pub const ESRCH: Errno = Errno(3);
pub const EINTR: Errno = Errno(4);
pub const EIO: Errno = Errno(5);
pub const ENXIO: Errno = Errno(6);
pub const E2BIG: Errno = Errno(7);
pub const ENOEXEC: Errno = Errno(8);
pub const EBADF: Errno = Errno(9);
pub const ECHILD: Errno = Errno(10);
pub const EAGAIN: Errno = Errno(11);
pub const ENOMEM: Errno = Errno(12);
pub const EACCES: Errno = Errno(13);
pub const EFAULT: Errno = Errno(14);
pub const EBUSY: Errno = Errno(16);
pub const EEXIST: Errno = Errno(17);
pub const EXDEV: Errno = Errno(18);
pub const ENODEV: Errno = Errno(19);
pub const ENOTDIR: Errno = Errno(20);
pub const EISDIR: Errno = Errno(21);
pub const EINVAL: Errno = Errno(22);
pub const ENFILE: Errno = Errno(23);
pub const EMFILE: Errno = Errno(24);
pub const ENOTTY: Errno = Errno(25);
pub const EFBIG: Errno = Errno(27);
pub const ENOSPC: Errno = Errno(28);
pub const ESPIPE: Errno = Errno(29);
pub const EROFS: Errno = Errno(30);
pub const EPIPE: Errno = Errno(32);
pub const ERANGE: Errno = Errno(34);
pub const EDEADLK: Errno = Errno(35);
pub const ENAMETOOLONG: Errno = Errno(36);
pub const ENOLCK: Errno = Errno(37);
pub const ENOSYS: Errno = Errno(38);
pub const ENOTEMPTY: Errno = Errno(39);
pub const ELOOP: Errno = Errno(40);
/// Alias for EAGAIN — same value on Linux.
pub const EWOULDBLOCK: Errno = Errno(11);
pub const ENOMSG: Errno = Errno(42);
pub const EPROTO: Errno = Errno(71);
pub const EOVERFLOW: Errno = Errno(75);
pub const EUSERS: Errno = Errno(87);
pub const ENOTSOCK: Errno = Errno(88);
pub const EDESTADDRREQ: Errno = Errno(89);
pub const EMSGSIZE: Errno = Errno(90);
pub const EPROTOTYPE: Errno = Errno(91);
pub const ENOPROTOOPT: Errno = Errno(92);
pub const EPROTONOSUPPORT: Errno = Errno(93);
pub const ESOCKTNOSUPPORT: Errno = Errno(94);
pub const EOPNOTSUPP: Errno = Errno(95);
pub const EAFNOSUPPORT: Errno = Errno(97);
pub const EADDRINUSE: Errno = Errno(98);
pub const EADDRNOTAVAIL: Errno = Errno(99);
pub const ENETDOWN: Errno = Errno(100);
pub const ENETUNREACH: Errno = Errno(101);
pub const ECONNABORTED: Errno = Errno(103);
pub const ECONNRESET: Errno = Errno(104);
pub const ENOBUFS: Errno = Errno(105);
pub const EISCONN: Errno = Errno(106);
pub const ENOTCONN: Errno = Errno(107);
pub const ETIMEDOUT: Errno = Errno(110);
pub const ECONNREFUSED: Errno = Errno(111);
pub const EHOSTUNREACH: Errno = Errno(113);
pub const EALREADY: Errno = Errno(114);
pub const EINPROGRESS: Errno = Errno(115);

static mut ERRNO_FALLBACK: i32 = 0;

#[inline]
pub fn errno_set(e: i32) {
    unsafe {
        if crate::thread::tls::tls_is_initialized() {
            *crate::thread::tcb::Tcb::errno_ptr() = e;
        } else {
            ERRNO_FALLBACK = e;
        }
    }
}

#[inline]
pub fn errno_get() -> i32 {
    unsafe {
        if crate::thread::tls::tls_is_initialized() {
            *crate::thread::tcb::Tcb::errno_ptr()
        } else {
            ERRNO_FALLBACK
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __errno_location() -> *mut i32 {
    if crate::thread::tls::tls_is_initialized() {
        crate::thread::tcb::Tcb::errno_ptr()
    } else {
        &raw mut ERRNO_FALLBACK
    }
}
