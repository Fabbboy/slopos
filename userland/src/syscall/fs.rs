//! File descriptor operations.
//!
//! This module provides two API layers:
//! - **Typed safe wrappers** (public): Return `SyscallResult<T>` for proper error handling
//! - **Raw C-ABI wrappers** (pub(crate)): Return raw `i64` for the libc compatibility layer
//!
//! Applications should use the typed APIs. The raw APIs are only for `libc/syscall.rs`.

use core::ffi::{CStr, c_char};

use super::RawFd;
use super::error::{SyscallResult, demux};
use super::numbers::*;
use super::raw::{syscall1, syscall2, syscall3};
use slopos_abi::syscall::{TIOCSCTTY, UserPollFd, UserTermios, UserTimeval};
use slopos_abi::{UserFsList, UserFsStat};

// =============================================================================
// Typed Safe Wrappers (Public API)
// =============================================================================

/// Open a file by path.
///
/// # Arguments
/// * `path` - Null-terminated path string
/// * `flags` - Open flags (USER_FS_OPEN_READ, USER_FS_OPEN_WRITE, etc.)
///
/// # Returns
/// File descriptor on success
///
/// # Errors
/// * `ENOENT` - File not found
/// * `EACCES` - Permission denied
/// * `EINVAL` - Invalid flags
#[inline(always)]
pub fn open_path(path: *const c_char, flags: u32) -> SyscallResult<RawFd> {
    let result = unsafe { syscall2(SYSCALL_FS_OPEN, path as u64, flags as u64) };
    demux(result).map(|v| v as RawFd)
}

/// Open a file using a CStr path.
#[inline(always)]
pub fn open_cstr(path: &CStr, flags: u32) -> SyscallResult<RawFd> {
    open_path(path.as_ptr(), flags)
}

/// Close a file descriptor.
///
/// # Errors
/// * `EBADF` - Invalid file descriptor
#[inline(always)]
pub fn close_fd(fd: RawFd) -> SyscallResult<()> {
    let result = unsafe { syscall1(SYSCALL_FS_CLOSE, fd as u64) };
    demux(result).map(|_| ())
}

/// Read from a file descriptor into a buffer.
///
/// # Returns
/// Number of bytes read, or 0 on EOF
///
/// # Errors
/// * `EBADF` - Invalid file descriptor
/// * `EIO` - I/O error
#[inline(always)]
pub fn read_slice(fd: RawFd, buf: &mut [u8]) -> SyscallResult<usize> {
    let result = unsafe {
        syscall3(
            SYSCALL_FS_READ,
            fd as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
    demux(result).map(|v| v as usize)
}

/// Write to a file descriptor from a buffer.
///
/// # Returns
/// Number of bytes written
///
/// # Errors
/// * `EBADF` - Invalid file descriptor
/// * `EIO` - I/O error
/// * `ENOSPC` - No space left on device
#[inline(always)]
pub fn write_slice(fd: RawFd, buf: &[u8]) -> SyscallResult<usize> {
    let result = unsafe {
        syscall3(
            SYSCALL_FS_WRITE,
            fd as u64,
            buf.as_ptr() as u64,
            buf.len() as u64,
        )
    };
    demux(result).map(|v| v as usize)
}

/// Get file status/metadata.
///
/// # Arguments
/// * `path` - Null-terminated path string
/// * `out_stat` - Output buffer for file status
///
/// # Errors
/// * `ENOENT` - File not found
#[inline(always)]
pub fn stat_path(path: *const c_char, out_stat: &mut UserFsStat) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_FS_STAT, path as u64, out_stat as *mut _ as u64) };
    demux(result).map(|_| ())
}

/// Create a directory.
///
/// # Arguments
/// * `path` - Null-terminated path string
///
/// # Errors
/// * `EEXIST` - Directory already exists
/// * `ENOENT` - Parent directory not found
/// * `ENOSPC` - No space left on device
#[inline(always)]
pub fn mkdir_path(path: *const c_char) -> SyscallResult<()> {
    let result = unsafe { syscall1(SYSCALL_FS_MKDIR, path as u64) };
    demux(result).map(|_| ())
}

/// Remove a file or empty directory.
///
/// # Arguments
/// * `path` - Null-terminated path string
///
/// # Errors
/// * `ENOENT` - File not found
/// * `EISDIR` - Is a non-empty directory
/// * `EBUSY` - File is in use
#[inline(always)]
pub fn unlink_path(path: *const c_char) -> SyscallResult<()> {
    let result = unsafe { syscall1(SYSCALL_FS_UNLINK, path as u64) };
    demux(result).map(|_| ())
}

/// Atomically rename/move a file or directory.
///
/// # Arguments
/// * `old_path` - Null-terminated old path string
/// * `new_path` - Null-terminated new path string
///
/// # Errors
/// * `ENOENT` - Source not found
/// * `EXDEV` - Cross-device rename
/// * `ENOTSUP` - Filesystem doesn't support rename
#[inline(always)]
pub fn rename(old_path: *const c_char, new_path: *const c_char) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_RENAME, old_path as u64, new_path as u64) };
    demux(result).map(|_| ())
}

/// List directory contents.
///
/// # Arguments
/// * `path` - Null-terminated path string
/// * `list` - Output buffer for directory entries
///
/// # Errors
/// * `ENOENT` - Directory not found
/// * `ENOTDIR` - Path is not a directory
#[inline(always)]
pub fn list_dir(path: *const c_char, list: &mut UserFsList) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_FS_LIST, path as u64, list as *mut _ as u64) };
    demux(result).map(|_| ())
}

#[inline(always)]
pub fn dup(fd: RawFd) -> SyscallResult<RawFd> {
    let result = unsafe { syscall1(SYSCALL_DUP, fd as u64) };
    demux(result).map(|v| v as RawFd)
}

#[inline(always)]
pub fn dup2(old_fd: RawFd, new_fd: RawFd) -> SyscallResult<RawFd> {
    let result = unsafe { syscall2(SYSCALL_DUP2, old_fd as u64, new_fd as u64) };
    demux(result).map(|v| v as RawFd)
}

#[inline(always)]
pub fn lseek(fd: RawFd, offset: i64, whence: u32) -> SyscallResult<i64> {
    let result = unsafe { syscall3(SYSCALL_LSEEK, fd as u64, offset as u64, whence as u64) };
    demux(result).map(|v| v as i64)
}

#[inline(always)]
pub fn pipe(fds: &mut [i32; 2]) -> SyscallResult<()> {
    let result = unsafe { syscall1(SYSCALL_PIPE, fds.as_mut_ptr() as u64) };
    demux(result).map(|_| ())
}

#[inline(always)]
pub fn pipe2(fds: &mut [i32; 2], flags: u32) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_PIPE2, fds.as_mut_ptr() as u64, flags as u64) };
    demux(result).map(|_| ())
}

#[inline(always)]
pub fn poll(fds: &mut [UserPollFd], timeout_ms: i64) -> SyscallResult<usize> {
    let result = unsafe {
        syscall3(
            SYSCALL_POLL,
            fds.as_mut_ptr() as u64,
            fds.len() as u64,
            timeout_ms as u64,
        )
    };
    demux(result).map(|v| v as usize)
}

#[inline(always)]
pub fn select(
    nfds: usize,
    readfds: *mut u8,
    writefds: *mut u8,
    exceptfds: *mut u8,
    timeout: *const UserTimeval,
) -> SyscallResult<usize> {
    let result = unsafe {
        super::raw::syscall5(
            SYSCALL_SELECT,
            nfds as u64,
            readfds as u64,
            writefds as u64,
            exceptfds as u64,
            timeout as u64,
        )
    };
    demux(result).map(|v| v as usize)
}

#[inline(always)]
pub fn tcgetpgrp(fd: RawFd) -> SyscallResult<u32> {
    let mut pgid = 0u32;
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCGPGRP,
            (&mut pgid as *mut u32) as u64,
        )
    };
    demux(result).map(|_| pgid)
}

#[inline(always)]
pub fn tcsetpgrp(fd: RawFd, pgid: u32) -> SyscallResult<()> {
    let mut target = pgid;
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCSPGRP,
            (&mut target as *mut u32) as u64,
        )
    };
    demux(result).map(|_| ())
}

#[inline(always)]
pub fn tiocsctty(fd: RawFd) -> SyscallResult<()> {
    let result = unsafe { syscall3(SYSCALL_IOCTL, fd as u64, TIOCSCTTY, 0) };
    demux(result).map(|_| ())
}

#[inline(always)]
pub fn tcgetattr(fd: RawFd) -> SyscallResult<UserTermios> {
    let mut t = UserTermios::default();
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TCGETS,
            (&mut t as *mut UserTermios) as u64,
        )
    };
    demux(result).map(|_| t)
}

#[inline(always)]
pub fn tcsetattr(fd: RawFd, t: &UserTermios) -> SyscallResult<()> {
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TCSETS,
            (t as *const UserTermios) as u64,
        )
    };
    demux(result).map(|_| ())
}
