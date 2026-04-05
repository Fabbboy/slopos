//! Platform Abstraction Layer for SlopOS.
//!
//! Every call through SlopOS is a gamble with the Wheel of Fate.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(missing_docs, nonstandard_style, dead_code)]

use crate::io;

pub mod futex;
pub mod os;

pub fn unsupported<T>() -> io::Result<T> {
    Err(unsupported_err())
}

pub fn unsupported_err() -> io::Error {
    io::const_error!(io::ErrorKind::Unsupported, "operation not supported on SlopOS yet")
}

pub fn abort_internal() -> ! {
    unsafe {
        slopos_abort();
    }
}

// SAFETY: must be called only once during runtime initialization.
// NOTE: this is not guaranteed to run, for example when Rust code is called externally.
pub unsafe fn init(argc: isize, argv: *const *const u8, _sigpipe: u8) {
    unsafe {
        crate::sys::args::init(argc, argv);
    }
}

// SAFETY: must be called only once during runtime cleanup.
// NOTE: this is not guaranteed to run, for example when the program aborts.
pub unsafe fn cleanup() {}

// ---------------------------------------------------------------------------
// errno
// ---------------------------------------------------------------------------

unsafe extern "C" {
    fn __errno_location() -> *mut i32;
    fn abort() -> !;
}

/// Read the thread-local errno set by C library functions.
pub fn errno() -> i32 {
    unsafe { __errno_location().read() }
}

// ---------------------------------------------------------------------------
// Error conversion helpers
// ---------------------------------------------------------------------------

/// Trait for C return values where -1 signals an error.
pub trait IsMinusOne {
    fn is_minus_one(&self) -> bool;
}

macro_rules! impl_is_minus_one {
    ($($t:ty),+) => { $(impl IsMinusOne for $t {
        fn is_minus_one(&self) -> bool { *self == -1 }
    })+ }
}
impl_is_minus_one!(i32, i64, isize);

/// Convert a C-ABI return value to `io::Result`.
///
/// Returns `Err` with errno when the value is -1.  Used by code that calls
/// slibc C functions (read, write, socket, …) which set errno on failure.
pub fn cvt<T: IsMinusOne>(t: T) -> io::Result<T> {
    if t.is_minus_one() {
        Err(io::Error::from_raw_os_error(errno()))
    } else {
        Ok(t)
    }
}

/// Like [`cvt`] but retries on `EINTR`.
pub fn cvt_r<T: IsMinusOne, F: FnMut() -> T>(mut f: F) -> io::Result<T> {
    loop {
        match cvt(f()) {
            Err(ref e) if e.raw_os_error() == Some(4 /* EINTR */) => {}
            other => return other,
        }
    }
}

/// Convert a raw-syscall return value (negated-errno convention) to
/// `io::Result`.  Kept for existing callers that bypass the C library.
pub fn cvt_syscall(ret: isize) -> io::Result<isize> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret)
    }
}

unsafe fn slopos_abort() -> ! {
    unsafe { abort() }
}
