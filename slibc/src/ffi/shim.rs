//! Safe wrappers over `ffi::syscalls::*` for use from tests.

use super::syscalls::{self, SloposStat};

pub fn slopos_clock_gettime(clk_id: u64, sec: &mut i64, nsec: &mut i64) -> i32 {
    // SAFETY: `sec` and `nsec` are live `&mut i64`s; pointers are
    // non-null, aligned, valid for one i64 write each.
    unsafe { syscalls::slopos_clock_gettime(clk_id, sec as *mut i64, nsec as *mut i64) }
}

pub fn slopos_stat(path: &[u8], stat_buf: &mut SloposStat) -> i32 {
    // SAFETY: `path` is a NUL-terminated byte slice; `stat_buf` is a
    // live `&mut SloposStat`. Both pointers valid for the call's duration.
    unsafe { syscalls::slopos_stat(path.as_ptr(), stat_buf as *mut SloposStat) }
}

pub fn slopos_lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    // SAFETY: extern reads no memory; arguments are plain integers.
    unsafe { syscalls::slopos_lseek(fd, offset, whence) }
}

pub fn slopos_futex_wake(addr: &u32, count: u32) -> i32 {
    // SAFETY: `addr` is a live `&u32`; pointer is non-null, aligned,
    // valid for one u32 read.
    unsafe { syscalls::slopos_futex_wake(addr as *const u32, count) }
}

pub fn slopos_pipe(fds: &mut [i32; 2]) -> i32 {
    // SAFETY: `fds` is a live `&mut [i32; 2]`; pointer is non-null,
    // aligned, valid for two i32 writes.
    unsafe { syscalls::slopos_pipe(fds.as_mut_ptr()) }
}

/// Safe wrapper for `crate::ffi::close` (already a safe extern; this
/// just keeps test imports tidy).
pub fn close(fd: i32) -> i32 {
    crate::ffi::close(fd)
}
