//! Thin syscall wrappers for the windowing platform layer.
//!
//! Uses `slopos_slibc::pal::raw::syscall*()` directly — the same pattern
//! that `slopos-protocol` uses for its internal `raw_poll()` and `timestamp_ms()`.

use slopos_abi::syscall::numbers::*;
use slopos_abi::syscall::types::UserPollFd;

/// Create a pipe pair with flags. Returns `(read_fd, write_fd)`.
pub fn pipe2(flags: u32) -> Result<(i32, i32), ()> {
    let mut raw = [0i32; 2];
    let result = unsafe {
        slopos_slibc::pal::raw::syscall2(SYSCALL_PIPE2, raw.as_mut_ptr() as u64, flags as u64)
    };
    if (result as i64) < 0 {
        return Err(());
    }
    Ok((raw[0], raw[1]))
}

/// Poll file descriptors for events.
pub fn poll(fds: &mut [UserPollFd], timeout_ms: i64) -> i32 {
    unsafe {
        slopos_slibc::pal::raw::syscall3(
            SYSCALL_POLL,
            fds.as_mut_ptr() as u64,
            fds.len() as u64,
            timeout_ms as u64,
        ) as i32
    }
}

/// Read from a file descriptor. Returns bytes read or negative errno.
pub fn read(fd: i32, buf: &mut [u8]) -> isize {
    unsafe {
        slopos_slibc::pal::raw::syscall3(
            SYSCALL_READ,
            fd as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as isize
    }
}

/// Write to a file descriptor. Returns bytes written or negative errno.
pub fn write(fd: i32, buf: &[u8]) -> isize {
    unsafe {
        slopos_slibc::pal::raw::syscall3(
            SYSCALL_WRITE,
            fd as u64,
            buf.as_ptr() as u64,
            buf.len() as u64,
        ) as isize
    }
}

/// Close a file descriptor.
pub fn close(fd: i32) -> i32 {
    unsafe { slopos_slibc::pal::raw::syscall1(SYSCALL_FS_CLOSE, fd as u64) as i32 }
}

/// Get monotonic time in milliseconds.
pub fn get_time_ms() -> u64 {
    unsafe { slopos_slibc::pal::raw::syscall0(SYSCALL_GET_TIME_MS) }
}

/// Sleep for the given number of milliseconds.
pub fn sleep_ms(ms: u64) {
    unsafe {
        slopos_slibc::pal::raw::syscall1(SYSCALL_SLEEP_MS, ms);
    }
}

/// Write a debug message to the console (fd-less TTY write).
pub fn tty_write(buf: &[u8]) {
    unsafe {
        slopos_slibc::pal::raw::syscall2(SYSCALL_WRITE, buf.as_ptr() as u64, buf.len() as u64);
    }
}

// ---------------------------------------------------------------------------
// memfd + mmap wrappers (fd-based shared memory)
// ---------------------------------------------------------------------------

/// Create an anonymous memory-backed file descriptor.
pub fn memfd_create(flags: u32) -> i32 {
    unsafe { slopos_slibc::pal::raw::syscall1(SYSCALL_MEMFD_CREATE, flags as u64) as i32 }
}

/// Set the size of a memfd.
pub fn ftruncate(fd: i32, size: u64) -> i32 {
    unsafe { slopos_slibc::pal::raw::syscall2(SYSCALL_FTRUNCATE, fd as u64, size) as i32 }
}

/// Map memory. Returns the virtual address, or 0/negative on failure.
pub fn mmap(addr: u64, length: u64, prot: u64, flags: u64, fd: i64, offset: u64) -> u64 {
    unsafe {
        slopos_slibc::pal::raw::syscall6(SYSCALL_MMAP, addr, length, prot, flags, fd as u64, offset)
    }
}

/// Unmap a previously mmap'd region.
pub fn munmap(addr: u64, length: u64) -> i32 {
    unsafe { slopos_slibc::pal::raw::syscall2(SYSCALL_MUNMAP, addr, length) as i32 }
}
