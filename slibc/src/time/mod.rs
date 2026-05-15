//! Time functions — the ticking clock of Sloptopia.
//! Time is a flat circle; the Wheel spins regardless.

#[allow(dead_code)]
pub(crate) mod shim;
pub mod tests;

use crate::errno::errno_set;
use crate::pal::{Pal, Sys};

// =============================================================================
// Clock constants
// =============================================================================

pub const CLOCK_REALTIME: i32 = 1;
pub const CLOCK_MONOTONIC: i32 = 0;

// =============================================================================
// Time structures
// =============================================================================

/// POSIX timespec — seconds and nanoseconds.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// POSIX timeval — seconds and microseconds.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

// =============================================================================
// Time functions
// =============================================================================

/// Get time from a specific clock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32 {
    if tp.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }

    let mut raw = [0u8; 16];
    match Sys::clock_gettime(clk_id as u64, raw.as_mut_ptr()) {
        Ok(()) => {
            (*tp).tv_sec = i64::from_le_bytes([
                raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
            ]);
            (*tp).tv_nsec = i64::from_le_bytes([
                raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
            ]);
            0
        }
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Get time of day — calls clock_gettime and converts to microseconds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gettimeofday(tv: *mut Timeval, _tz: *mut u8) -> i32 {
    if tv.is_null() {
        return 0; // POSIX permits null tv
    }

    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = clock_gettime(CLOCK_REALTIME, &mut ts);
    if ret != 0 {
        return ret;
    }

    (*tv).tv_sec = ts.tv_sec;
    (*tv).tv_usec = ts.tv_nsec / 1000;
    0
}

/// Return seconds since the epoch.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time(tloc: *mut i64) -> i64 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = clock_gettime(CLOCK_REALTIME, &mut ts);
    if ret != 0 {
        return -1;
    }

    if !tloc.is_null() {
        *tloc = ts.tv_sec;
    }
    ts.tv_sec
}

/// Sleep for the specified duration. Converts to milliseconds and calls
/// the kernel's `sleep_ms` syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nanosleep(req: *const Timespec, _rem: *mut Timespec) -> i32 {
    if req.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }

    let ms = ((*req).tv_sec as u64) * 1000 + ((*req).tv_nsec as u64) / 1_000_000;
    Sys::sleep_ms(ms);

    // Zero out remainder (we don't support interrupting sleep)
    if !_rem.is_null() {
        (*_rem).tv_sec = 0;
        (*_rem).tv_nsec = 0;
    }
    0
}

/// Sleep for `usec` microseconds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn usleep(usec: u32) -> i32 {
    let ts = Timespec {
        tv_sec: (usec / 1_000_000) as i64,
        tv_nsec: ((usec % 1_000_000) as i64) * 1000,
    };
    nanosleep(&ts, core::ptr::null_mut())
}

/// Sleep for `seconds` seconds. Returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sleep(seconds: u32) -> u32 {
    let ts = Timespec {
        tv_sec: seconds as i64,
        tv_nsec: 0,
    };
    nanosleep(&ts, core::ptr::null_mut());
    0
}
