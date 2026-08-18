//! DNS resolution — getaddrinfo and friends.

use crate::mem::malloc;
use crate::pal::{Pal, Sys};
use crate::string::u_strlen;

use super::addr::{AF_INET, SOCK_STREAM, SockAddr, SockAddrIn};

pub const EAI_NONAME: i32 = -2;
pub const EAI_AGAIN: i32 = -3;
pub const EAI_FAIL: i32 = -4;
pub const EAI_MEMORY: i32 = -10;
pub const EAI_SYSTEM: i32 = -11;

pub const AI_PASSIVE: i32 = 0x01;
pub const AI_NUMERICHOST: i32 = 0x04;

/// Result node from getaddrinfo — POSIX `struct addrinfo`.
#[repr(C)]
pub struct AddrInfo {
    pub ai_flags: i32,
    pub ai_family: i32,
    pub ai_socktype: i32,
    pub ai_protocol: i32,
    pub ai_addrlen: u32,
    pub ai_addr: *mut SockAddr,
    pub ai_canonname: *mut u8,
    pub ai_next: *mut AddrInfo,
}

/// Simplified getaddrinfo — resolves a hostname or dotted-decimal IP.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getaddrinfo(
    node: *const u8,
    _service: *const u8,
    hints: *const AddrInfo,
    res: *mut *mut AddrInfo,
) -> i32 {
    if res.is_null() {
        return EAI_SYSTEM;
    }
    *res = core::ptr::null_mut();

    if node.is_null() {
        return EAI_NONAME;
    }

    let (sock_type, protocol) = if !hints.is_null() {
        ((*hints).ai_socktype, (*hints).ai_protocol)
    } else {
        (SOCK_STREAM, 0)
    };

    let addr_u32 = super::addr::inet_addr(node);
    let resolved_addr = if addr_u32 != super::addr::INADDR_NONE {
        addr_u32
    } else {
        let hostname_len = u_strlen(node);
        let mut result_buf = [0u8; 4];
        match Sys::resolve(node, hostname_len, result_buf.as_mut_ptr()) {
            Ok(()) => u32::from_ne_bytes(result_buf),
            Err(_) => return EAI_NONAME,
        }
    };

    let alloc_size = core::mem::size_of::<AddrInfo>() + core::mem::size_of::<SockAddrIn>();
    let ptr = malloc::alloc(alloc_size);
    if ptr.is_null() {
        return EAI_MEMORY;
    }

    let ai = ptr as *mut AddrInfo;
    let sa = (ptr as *mut u8).add(core::mem::size_of::<AddrInfo>()) as *mut SockAddrIn;

    core::ptr::write_bytes(sa, 0, 1);
    (*sa).sin_family = AF_INET as u16;
    (*sa).sin_port = 0; // Service port parsing is not implemented
    (*sa).sin_addr = resolved_addr;

    (*ai).ai_flags = 0;
    (*ai).ai_family = AF_INET;
    (*ai).ai_socktype = sock_type;
    (*ai).ai_protocol = protocol;
    (*ai).ai_addrlen = core::mem::size_of::<SockAddrIn>() as u32;
    (*ai).ai_addr = sa as *mut SockAddr;
    (*ai).ai_canonname = core::ptr::null_mut();
    (*ai).ai_next = core::ptr::null_mut();

    *res = ai;
    0
}

/// Free the linked list allocated by `getaddrinfo`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn freeaddrinfo(res: *mut AddrInfo) {
    let mut cur = res;
    while !cur.is_null() {
        let next = (*cur).ai_next;
        // AddrInfo and SockAddrIn share one allocation; `ai_addr` points inside
        // it, so only the AddrInfo pointer is freed.
        malloc::dealloc(cur as *mut core::ffi::c_void);
        cur = next;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gai_strerror(errcode: i32) -> *const u8 {
    match errcode {
        0 => b"Success\0".as_ptr(),
        EAI_NONAME => b"Name or service not known\0".as_ptr(),
        EAI_AGAIN => b"Temporary failure in name resolution\0".as_ptr(),
        EAI_FAIL => b"Non-recoverable failure in name resolution\0".as_ptr(),
        EAI_MEMORY => b"Memory allocation failure\0".as_ptr(),
        EAI_SYSTEM => b"System error\0".as_ptr(),
        _ => b"Unknown error\0".as_ptr(),
    }
}
