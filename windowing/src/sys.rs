//! Thin syscall wrappers for the windowing platform layer.
//!
//! Routes every syscall through the safe [`slopos_slibc::pal::Sys`]
//! façade so this module stays `unsafe`-free. Each helper translates
//! `Result<…, Errno>` back into the simpler `i32 / isize / u64`
//! return shape that the surrounding event-loop expects.

use slopos_abi::syscall::types::UserPollFd;
use slopos_slibc::pal::{Pal, Sys};

/// Create a pipe pair with flags. Returns `(read_fd, write_fd)`.
pub fn pipe2(flags: u32) -> Result<(i32, i32), ()> {
    let mut raw = [0i32; 2];
    match Sys::pipe2(&mut raw as *mut [i32; 2], flags) {
        Ok(()) => Ok((raw[0], raw[1])),
        Err(_) => Err(()),
    }
}

/// Poll file descriptors for events.
pub fn poll(fds: &mut [UserPollFd], timeout_ms: i64) -> i32 {
    Sys::poll(
        fds.as_mut_ptr() as *mut u8,
        fds.len() as u32,
        timeout_ms as i32,
    )
    .unwrap_or(-1)
}

/// Read from a file descriptor. Returns bytes read or negative errno.
pub fn read(fd: i32, buf: &mut [u8]) -> isize {
    match Sys::read(fd, buf.as_mut_ptr(), buf.len()) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

/// Write to a file descriptor. Returns bytes written or negative errno.
pub fn write(fd: i32, buf: &[u8]) -> isize {
    match Sys::write(fd, buf.as_ptr(), buf.len()) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

/// Close a file descriptor.
pub fn close(fd: i32) -> i32 {
    match Sys::close(fd) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

/// Get monotonic time in milliseconds.
pub fn get_time_ms() -> u64 {
    Sys::get_time_ms()
}

/// Sleep for the given number of milliseconds.
pub fn sleep_ms(ms: u64) {
    Sys::sleep_ms(ms);
}

/// Write a debug message to the console (fd-less TTY write).
pub fn tty_write(buf: &[u8]) {
    // Routed to fd 1 (stdout). Match the original best-effort
    // semantics: any error is silently discarded.
    let _ = Sys::write(1, buf.as_ptr(), buf.len());
}

// ---------------------------------------------------------------------------
// memfd + mmap wrappers (fd-based shared memory)
// ---------------------------------------------------------------------------

/// Create an anonymous memory-backed file descriptor.
pub fn memfd_create(flags: u32) -> i32 {
    Sys::memfd_create(flags).unwrap_or(-1)
}

/// Set the size of a memfd.
pub fn ftruncate(fd: i32, size: u64) -> i32 {
    match Sys::ftruncate(fd, size) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

/// Map memory. Returns the virtual address, or 0/negative on failure.
pub fn mmap(addr: u64, length: u64, prot: u64, flags: u64, fd: i64, offset: u64) -> u64 {
    match Sys::mmap(
        addr as *mut u8,
        length as usize,
        prot,
        flags,
        fd as i32,
        offset,
    ) {
        Ok(p) => p as u64,
        Err(_) => 0,
    }
}

/// Unmap a previously mmap'd region.
pub fn munmap(addr: u64, length: u64) -> i32 {
    match Sys::munmap(addr as *mut u8, length as usize) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}
