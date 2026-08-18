//! Safe wrappers over `io::poll` and `io::misc` for use from tests.

use super::misc;
use super::poll::{self, FdSet};

pub fn fd_zero(set: &mut FdSet) {
    // SAFETY: `set` is a live `&mut FdSet`; pointer is non-null and valid for writes.
    unsafe { poll::fd_zero(set as *mut FdSet) }
}

pub fn fd_set(fd: i32, set: &mut FdSet) {
    // SAFETY: `set` is a live `&mut FdSet`; pointer is non-null and valid for writes.
    unsafe { poll::fd_set(fd, set as *mut FdSet) }
}

pub fn fd_clr(fd: i32, set: &mut FdSet) {
    // SAFETY: `set` is a live `&mut FdSet`; pointer is non-null and valid for writes.
    unsafe { poll::fd_clr(fd, set as *mut FdSet) }
}

pub fn fd_isset(fd: i32, set: &FdSet) -> bool {
    // SAFETY: `set` is a live `&FdSet`; pointer is non-null and valid for reads.
    unsafe { poll::fd_isset(fd, set as *const FdSet) }
}

/// Returns true iff every `fd_*` helper's null-pointer path returned its
/// documented sentinel.
pub fn fd_macros_null_safe() -> bool {
    // SAFETY: each fd_* helper documents a null pointer as a no-op / false
    // return.
    unsafe {
        poll::fd_set(5, core::ptr::null_mut());
        poll::fd_clr(5, core::ptr::null_mut());
        poll::fd_zero(core::ptr::null_mut());
        !poll::fd_isset(5, core::ptr::null())
    }
}

pub fn pipe(pipefd: &mut [i32; 2]) -> i32 {
    // SAFETY: `pipefd` is a live `&mut [i32; 2]`; pointer is non-null, aligned
    // and valid for 2 i32 writes.
    unsafe { misc::pipe(pipefd.as_mut_ptr()) }
}

pub fn dup(oldfd: i32) -> i32 {
    // SAFETY: extern reads no memory; argument is a plain integer.
    unsafe { misc::dup(oldfd) }
}

pub fn dup2(oldfd: i32, newfd: i32) -> i32 {
    // SAFETY: extern reads no memory; arguments are plain integers.
    unsafe { misc::dup2(oldfd, newfd) }
}

pub fn isatty(fd: i32) -> i32 {
    // SAFETY: extern reads no memory; argument is a plain integer.
    unsafe { misc::isatty(fd) }
}

pub fn umask(mask: u32) -> u32 {
    misc::umask(mask)
}

pub fn access_cstr(path: &[u8], mode: i32) -> i32 {
    // SAFETY: caller provides a NUL-terminated byte slice; `access` reads
    // bytes until the NUL and never beyond.
    unsafe { misc::access(path.as_ptr(), mode) }
}

pub fn chmod_cstr(path: &[u8], mode: u32) -> i32 {
    // SAFETY: caller provides a NUL-terminated byte slice; `chmod` reads bytes
    // until the NUL and never beyond.
    unsafe { misc::chmod(path.as_ptr(), mode) }
}

pub fn close(fd: i32) -> i32 {
    crate::ffi::close(fd)
}
