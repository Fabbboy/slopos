//! File descriptor operations, as typed wrappers returning `SyscallResult<T>`.

use core::ffi::{CStr, c_char};

use super::RawFd;
use super::error::{SyscallResult, demux};
use super::numbers::*;
use super::raw::{syscall1, syscall2, syscall3};
use slopos_abi::syscall::{
    TIOCGPTPEER, TIOCGSID, TIOCGWINSZ, TIOCSCTTY, TIOCSWINSZ, UserPollFd, UserTermios, UserTimeval,
    UserWinsize,
};
use slopos_abi::{UserFsList, UserFsStat};
use slopos_slibc::pal::{Pal, Sys};

/// Open a file by path.
///
/// # Errors
/// * `ENOENT` - File not found
/// * `EACCES` - Permission denied
/// * `EINVAL` - Invalid flags
#[inline(always)]
pub fn open_path(path: *const c_char, flags: u32) -> SyscallResult<super::OwnedFd> {
    Sys::open(path as *const u8, flags as i32, 0)
        // SAFETY: fd is a valid descriptor just returned by the kernel.
        .map(|fd| unsafe { super::OwnedFd::from_raw(fd as RawFd) })
        .map_err(Into::into)
}

#[inline(always)]
pub fn open_cstr(path: &CStr, flags: u32) -> SyscallResult<super::OwnedFd> {
    open_path(path.as_ptr(), flags)
}

/// Escape hatch for well-known fds (0/1/2) and fds taken out of an `OwnedFd`;
/// prefer dropping the `OwnedFd`.
#[inline(always)]
pub fn close_fd_raw(fd: RawFd) -> SyscallResult<()> {
    Sys::close(fd).map_err(Into::into)
}

/// Consumes the handle so `Drop` cannot double-close. On failure the fd is
/// still consumed: the kernel either closed it or it was invalid.
#[inline(always)]
pub fn close_fd(fd: super::OwnedFd) -> SyscallResult<()> {
    close_fd_raw(fd.into_raw())
}

/// Read from a file descriptor into a buffer. Returns 0 at EOF.
///
/// # Errors
/// * `EBADF` - Invalid file descriptor
/// * `EIO` - I/O error
#[inline(always)]
pub fn read_slice(fd: RawFd, buf: &mut [u8]) -> SyscallResult<usize> {
    Sys::read(fd, buf.as_mut_ptr(), buf.len()).map_err(Into::into)
}

/// Write to a file descriptor from a buffer.
///
/// # Errors
/// * `EBADF` - Invalid file descriptor
/// * `EIO` - I/O error
/// * `ENOSPC` - No space left on device
#[inline(always)]
pub fn write_slice(fd: RawFd, buf: &[u8]) -> SyscallResult<usize> {
    Sys::write(fd, buf.as_ptr(), buf.len()).map_err(Into::into)
}

/// Get file status/metadata.
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
/// # Errors
/// * `ENOENT` - File not found
/// * `EISDIR` - Is a non-empty directory
/// * `EBUSY` - File is in use
#[inline(always)]
pub fn unlink_path(path: *const c_char) -> SyscallResult<()> {
    Sys::unlink(path as *const u8).map_err(Into::into)
}

/// Atomically rename/move a file or directory.
///
/// # Errors
/// * `ENOENT` - Source not found
/// * `EXDEV` - Cross-device rename
/// * `ENOTSUP` - Filesystem doesn't support rename
#[inline(always)]
pub fn rename(old_path: *const c_char, new_path: *const c_char) -> SyscallResult<()> {
    Sys::rename(old_path as *const u8, new_path as *const u8).map_err(Into::into)
}

/// List directory contents.
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
pub fn dup(fd: RawFd) -> SyscallResult<super::OwnedFd> {
    Sys::dup(fd)
        // SAFETY: v is a valid fd just returned by the kernel.
        .map(|v| unsafe { super::OwnedFd::from_raw(v as RawFd) })
        .map_err(Into::into)
}

/// Closes whatever was at `new_fd`. The `new_fd` slot is a raw alias
/// afterwards: no `OwnedFd` tracks its lifetime.
#[inline(always)]
pub fn dup2(old_fd: RawFd, new_fd: RawFd) -> SyscallResult<RawFd> {
    Sys::dup2(old_fd, new_fd)
        .map(|v| v as RawFd)
        .map_err(Into::into)
}

#[inline(always)]
pub fn lseek(fd: RawFd, offset: i64, whence: u32) -> SyscallResult<i64> {
    Sys::lseek(fd, offset, whence as i32).map_err(Into::into)
}

/// Returns `(read_end, write_end)`.
#[inline(always)]
pub fn pipe() -> SyscallResult<(super::OwnedFd, super::OwnedFd)> {
    let mut raw = [0i32; 2];
    Sys::pipe(&mut raw as *mut [i32; 2]).map_err(super::SyscallError::from)?;
    // SAFETY: raw fds are valid descriptors just returned by the kernel.
    Ok(unsafe {
        (
            super::OwnedFd::from_raw(raw[0]),
            super::OwnedFd::from_raw(raw[1]),
        )
    })
}

/// Returns `(read_end, write_end)`.
#[inline(always)]
pub fn pipe2(flags: u32) -> SyscallResult<(super::OwnedFd, super::OwnedFd)> {
    let mut raw = [0i32; 2];
    let result = unsafe { syscall2(SYSCALL_PIPE2, raw.as_mut_ptr() as u64, flags as u64) };
    demux(result)?;
    // SAFETY: raw fds are valid descriptors just returned by the kernel.
    Ok(unsafe {
        (
            super::OwnedFd::from_raw(raw[0]),
            super::OwnedFd::from_raw(raw[1]),
        )
    })
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

/// Session id owning the terminal on `fd` (TIOCGSID ioctl).
///
/// Fails when `fd` is not a terminal or the terminal has no session, which is
/// how a shell tells an unclaimed terminal from one already in use.
#[inline(always)]
pub fn tcgetsid(fd: RawFd) -> SyscallResult<u32> {
    let mut sid = 0u32;
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCGSID,
            (&mut sid as *mut u32) as u64,
        )
    };
    demux(result).map(|_| sid)
}

/// Open the PTY slave peer of a master FD (TIOCGPTPEER ioctl). The new fd
/// shares the slave's open state with every other slave fd.
#[inline(always)]
pub fn ioctl_tiocgptpeer(master_fd: RawFd) -> SyscallResult<super::OwnedFd> {
    let result = unsafe { syscall3(SYSCALL_IOCTL, master_fd as u64, TIOCGPTPEER, 0) };
    // SAFETY: v is a valid fd just returned by the kernel.
    demux(result).map(|v| unsafe { super::OwnedFd::from_raw(v as i32) })
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

/// POSIX `isatty(3)`. The TCGETS probe is exact, not approximate:
/// `syscall_ioctl` resolves the descriptor through `file_get_tty_index`, which
/// yields nothing unless the file's kind is `Tty`.
#[inline]
pub fn isatty(fd: RawFd) -> bool {
    tcgetattr(fd).is_ok()
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

#[inline(always)]
pub fn tiocgwinsz(fd: RawFd) -> SyscallResult<UserWinsize> {
    let mut ws = UserWinsize::default();
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCGWINSZ,
            (&mut ws as *mut UserWinsize) as u64,
        )
    };
    demux(result).map(|_| ws)
}

/// The kernel raises SIGWINCH to the slave foreground process group when the
/// row/col dimensions change.
#[inline(always)]
pub fn tiocswinsz(fd: RawFd, ws: &UserWinsize) -> SyscallResult<()> {
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCSWINSZ,
            (ws as *const UserWinsize) as u64,
        )
    };
    demux(result).map(|_| ())
}

/// Works on any fd type (pipes, sockets, files).
#[inline(always)]
pub fn set_fd_nonblocking(fd: RawFd) -> SyscallResult<()> {
    use super::error::SyscallError;
    use slopos_abi::syscall::{F_GETFL, F_SETFL, O_NONBLOCK};
    let current = Sys::fcntl(fd, F_GETFL as i32, 0).map_err(SyscallError::from)?;
    let _ = Sys::fcntl(fd, F_SETFL as i32, (current as u64) | O_NONBLOCK)
        .map_err(SyscallError::from)?;
    Ok(())
}

/// Spawned children never inherit a `FD_CLOEXEC` descriptor (spawn is
/// fork+exec in one step), and `exec` strips it from forked children.
#[inline(always)]
pub fn set_fd_cloexec(fd: RawFd) -> SyscallResult<()> {
    use super::error::SyscallError;
    use slopos_abi::syscall::{F_GETFD, F_SETFD, FD_CLOEXEC};
    let current = Sys::fcntl(fd, F_GETFD as i32, 0).map_err(SyscallError::from)?;
    let _ = Sys::fcntl(fd, F_SETFD as i32, (current as u64) | FD_CLOEXEC)
        .map_err(SyscallError::from)?;
    Ok(())
}
