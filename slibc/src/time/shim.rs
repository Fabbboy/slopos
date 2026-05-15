//! Safe wrappers over `time::*` for use from tests.

use super::{Timespec, Timeval};

pub fn clock_gettime(clk_id: i32, tp: &mut Timespec) -> i32 {
    // SAFETY: `tp` is a live `&mut Timespec`; pointer is non-null,
    // aligned, valid for one Timespec write.
    unsafe { super::clock_gettime(clk_id, tp as *mut Timespec) }
}

pub fn clock_gettime_null_tp(clk_id: i32) -> i32 {
    // SAFETY: `clock_gettime` documents null `tp` as a defined input
    // that returns -1.
    unsafe { super::clock_gettime(clk_id, core::ptr::null_mut()) }
}

pub fn gettimeofday(tv: &mut Timeval) -> i32 {
    // SAFETY: `tv` is a live `&mut Timeval`. The `_tz` argument is
    // ignored by the implementation and accepts null.
    unsafe { super::gettimeofday(tv as *mut Timeval, core::ptr::null_mut()) }
}

pub fn gettimeofday_null_tv() -> i32 {
    // SAFETY: `gettimeofday` documents null `tv` as a defined input
    // that returns 0 (per slibc's implementation).
    unsafe { super::gettimeofday(core::ptr::null_mut(), core::ptr::null_mut()) }
}

pub fn time(tloc: Option<&mut i64>) -> i64 {
    match tloc {
        // SAFETY: `t` is a live `&mut i64`; pointer is non-null and
        // valid for one i64 write.
        Some(t) => unsafe { super::time(t as *mut i64) },
        // SAFETY: null `tloc` is documented as "do not store".
        None => unsafe { super::time(core::ptr::null_mut()) },
    }
}

pub fn nanosleep(req: &Timespec) -> i32 {
    // SAFETY: `req` is a live `&Timespec`; pointer is non-null and
    // valid for one Timespec read. `_rem` accepts null.
    unsafe { super::nanosleep(req as *const Timespec, core::ptr::null_mut()) }
}

pub fn nanosleep_null_req() -> i32 {
    // SAFETY: `nanosleep` documents null `req` as a defined input
    // that returns -1.
    unsafe { super::nanosleep(core::ptr::null(), core::ptr::null_mut()) }
}

pub fn usleep(usec: u32) -> i32 {
    // SAFETY: extern reads no memory; argument is a plain integer.
    unsafe { super::usleep(usec) }
}

pub fn sleep(seconds: u32) -> u32 {
    // SAFETY: extern reads no memory; argument is a plain integer.
    unsafe { super::sleep(seconds) }
}
