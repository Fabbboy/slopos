//! Miscellaneous POSIX file operations — the everyday runes of Sloptopia.

use crate::errno::{ENOSYS, errno_set};
use crate::pal::{Pal, Sys};

// =============================================================================
// File existence / permissions
// =============================================================================

/// Check file accessibility. Stub: calls stat, returns 0 if file exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(path: *const u8, _mode: i32) -> i32 {
    if path.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    let mut stat_buf = [0u8; 256];
    match Sys::stat(path, stat_buf.as_mut_ptr()) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Set file creation mask. Stub: returns 0o022 (no kernel support needed for basic use).
#[unsafe(no_mangle)]
pub extern "C" fn umask(_mask: u32) -> u32 {
    0o022
}

/// Change file permissions. Stub: returns -1 with ENOSYS (kernel lacks chmod).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chmod(_path: *const u8, _mode: u32) -> i32 {
    errno_set(ENOSYS.raw());
    -1
}

// =============================================================================
// File descriptor operations
// =============================================================================

/// Create a pipe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pipe(pipefd: *mut i32) -> i32 {
    if pipefd.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    let fds_arr = &mut *(pipefd as *mut [i32; 2]);
    match Sys::pipe(fds_arr) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Duplicate a file descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dup(oldfd: i32) -> i32 {
    match Sys::dup(oldfd) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Duplicate a file descriptor to a specific number.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dup2(oldfd: i32, newfd: i32) -> i32 {
    match Sys::dup2(oldfd, newfd) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// File control operations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fcntl(fd: i32, cmd: i32, arg: i64) -> i32 {
    match Sys::fcntl(fd, cmd, arg as u64) {
        Ok(ret) => ret,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Check whether a file descriptor refers to a terminal.
/// Returns 1 if it is a terminal, 0 otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isatty(fd: i32) -> i32 {
    // Try TCGETS ioctl — if it succeeds, fd is a terminal
    let mut buf = [0u8; 64];
    match Sys::ioctl(fd, slopos_abi::syscall::TCGETS, buf.as_mut_ptr() as u64) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}
