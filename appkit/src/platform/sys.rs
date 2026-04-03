//! Thin syscall wrappers for the appkit platform layer.
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

/// Create a shared memory region. Returns a token (0 on failure).
pub fn shm_create(size: u64, flags: u32) -> u32 {
    unsafe { slopos_slibc::pal::raw::syscall2(SYSCALL_SHM_CREATE, size, flags as u64) as u32 }
}

/// Map a shared memory region. Returns virtual address (0 or negative on failure).
pub fn shm_map(token: u32, access: u32) -> u64 {
    unsafe { slopos_slibc::pal::raw::syscall2(SYSCALL_SHM_MAP, token as u64, access as u64) }
}

/// Unmap a shared memory region.
pub unsafe fn shm_unmap(virt_addr: u64) -> i64 {
    unsafe { slopos_slibc::pal::raw::syscall1(SYSCALL_SHM_UNMAP, virt_addr) as i64 }
}

/// Destroy a shared memory region.
pub fn shm_destroy(token: u32) -> i64 {
    unsafe { slopos_slibc::pal::raw::syscall1(SYSCALL_SHM_DESTROY, token as u64) as i64 }
}
