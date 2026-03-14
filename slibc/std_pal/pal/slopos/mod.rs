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

// Error conversion helpers
pub fn cvt(ret: isize) -> io::Result<isize> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret)
    }
}

pub fn cvt_i32(ret: i32) -> io::Result<i32> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret))
    } else {
        Ok(ret)
    }
}

unsafe extern "C" {
    fn abort() -> !;
}

unsafe fn slopos_abort() -> ! {
    unsafe { abort() }
}
