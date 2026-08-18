//! poll() and select() — multiplexed I/O.

use crate::errno::errno_set;
use crate::pal::{Pal, Sys};

pub const POLLIN: i16 = 0x0001;
pub const POLLPRI: i16 = 0x0002;
pub const POLLOUT: i16 = 0x0004;
pub const POLLERR: i16 = 0x0008;
pub const POLLHUP: i16 = 0x0010;
pub const POLLNVAL: i16 = 0x0020;

/// POSIX `struct pollfd`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Pollfd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

/// `fd_set` for select() — supports up to 1024 file descriptors.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FdSet {
    pub fds_bits: [u64; 16],
}

impl Default for FdSet {
    fn default() -> Self {
        Self { fds_bits: [0; 16] }
    }
}

const FD_SETSIZE: usize = 1024;
const BITS_PER_WORD: usize = 64;

#[inline]
pub unsafe fn fd_zero(set: *mut FdSet) {
    if !set.is_null() {
        core::ptr::write_bytes(set, 0, 1);
    }
}

#[inline]
pub unsafe fn fd_set(fd: i32, set: *mut FdSet) {
    let fd = fd as usize;
    if !set.is_null() && fd < FD_SETSIZE {
        (*set).fds_bits[fd / BITS_PER_WORD] |= 1u64 << (fd % BITS_PER_WORD);
    }
}

#[inline]
pub unsafe fn fd_clr(fd: i32, set: *mut FdSet) {
    let fd = fd as usize;
    if !set.is_null() && fd < FD_SETSIZE {
        (*set).fds_bits[fd / BITS_PER_WORD] &= !(1u64 << (fd % BITS_PER_WORD));
    }
}

#[inline]
pub unsafe fn fd_isset(fd: i32, set: *const FdSet) -> bool {
    let fd = fd as usize;
    if set.is_null() || fd >= FD_SETSIZE {
        return false;
    }
    ((*set).fds_bits[fd / BITS_PER_WORD] & (1u64 << (fd % BITS_PER_WORD))) != 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn poll(fds: *mut Pollfd, nfds: u32, timeout: i32) -> i32 {
    match Sys::poll(fds as *mut u8, nfds, timeout) {
        Ok(n) => n,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn select(
    nfds: i32,
    readfds: *mut FdSet,
    writefds: *mut FdSet,
    exceptfds: *mut FdSet,
    timeout: *mut crate::time::Timeval,
) -> i32 {
    match Sys::select(
        nfds,
        readfds as *mut u8,
        writefds as *mut u8,
        exceptfds as *mut u8,
        timeout as *mut u8,
    ) {
        Ok(n) => n,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}
