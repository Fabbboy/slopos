//! POSIX socket API — the network runes of Sloptopia.
//! Each packet sent is a prayer to the Wheel; each received is a blessing.

pub mod addr;
pub mod dns;
pub mod tests;

use crate::errno::{ENOSYS, errno_set};
use crate::pal::{Pal, Sys};

pub use addr::*;

// =============================================================================
// Socket creation and connection
// =============================================================================

/// Create a socket endpoint for communication.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn socket(domain: i32, sock_type: i32, protocol: i32) -> i32 {
    match Sys::socket(domain, sock_type, protocol) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Bind a name to a socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bind(fd: i32, addr: *const SockAddr, addrlen: u32) -> i32 {
    match Sys::bind(fd, addr as *const u8, addrlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Listen for connections on a socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn listen(fd: i32, backlog: i32) -> i32 {
    match Sys::listen(fd, backlog) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Accept a connection on a socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn accept(fd: i32, addr: *mut SockAddr, addrlen: *mut u32) -> i32 {
    match Sys::accept(fd, addr as *mut u8, addrlen) {
        Ok(new_fd) => new_fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Initiate a connection on a socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn connect(fd: i32, addr: *const SockAddr, addrlen: u32) -> i32 {
    match Sys::connect(fd, addr as *const u8, addrlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

// =============================================================================
// Data transfer
// =============================================================================

/// Send a message on a socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn send(fd: i32, buf: *const u8, len: usize, flags: i32) -> isize {
    match Sys::send(fd, buf, len, flags) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Receive a message from a socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize {
    match Sys::recv(fd, buf, len, flags) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Send a message on a socket to a specific destination.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sendto(
    fd: i32,
    buf: *const u8,
    len: usize,
    flags: i32,
    dest_addr: *const SockAddr,
    addrlen: u32,
) -> isize {
    match Sys::sendto(fd, buf, len, flags, dest_addr as *const u8, addrlen) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Receive a message from a socket with source address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn recvfrom(
    fd: i32,
    buf: *mut u8,
    len: usize,
    flags: i32,
    src_addr: *mut SockAddr,
    addrlen: *mut u32,
) -> isize {
    match Sys::recvfrom(fd, buf, len, flags, src_addr as *mut u8, addrlen) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

// =============================================================================
// Socket options
// =============================================================================

/// Set options on a socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: *const u8,
    optlen: u32,
) -> i32 {
    match Sys::setsockopt(fd, level, optname, optval, optlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Get options on a socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: *mut u8,
    optlen: *mut u32,
) -> i32 {
    match Sys::getsockopt(fd, level, optname, optval, optlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Shut down part of a full-duplex connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shutdown(fd: i32, how: i32) -> i32 {
    match Sys::shutdown(fd, how) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

// =============================================================================
// Peer / local address (stubs — kernel lacks these syscalls)
// =============================================================================

/// Get name of connected peer socket. Stub: returns -1 with ENOSYS.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpeername(_fd: i32, _addr: *mut SockAddr, _addrlen: *mut u32) -> i32 {
    errno_set(ENOSYS.raw());
    -1
}

/// Get socket name. Stub: returns -1 with ENOSYS.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getsockname(_fd: i32, _addr: *mut SockAddr, _addrlen: *mut u32) -> i32 {
    errno_set(ENOSYS.raw());
    -1
}
