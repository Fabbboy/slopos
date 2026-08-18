//! Safe wrappers over `net::addr` and `net::dns` for use from tests.

use super::addr::{self, INADDR_NONE};
use super::dns::{self, AddrInfo};

/// Parse a NUL-terminated dotted-quad. Returns `INADDR_NONE` for
/// invalid input (matching `inet_addr` semantics).
pub fn inet_addr_cstr(s: &[u8]) -> u32 {
    // SAFETY: `s` is NUL-terminated; `inet_addr` reads no further than the NUL.
    unsafe { addr::inet_addr(s.as_ptr()) }
}

pub fn inet_addr_null() -> u32 {
    // SAFETY: `inet_addr` documents a null `cp` as a defined input.
    unsafe { addr::inet_addr(core::ptr::null()) }
}

/// Length of `inet_ntoa(addr)`'s output excluding the NUL; `None` if the
/// returned pointer is null.
pub fn inet_ntoa_len(addr: u32) -> Option<usize> {
    // SAFETY: `inet_ntoa` returns a NUL-terminated static buffer valid until
    // the next call.
    unsafe {
        let ptr = addr::inet_ntoa(addr);
        if ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        let mut p = ptr;
        while *p != 0 {
            len += 1;
            p = p.add(1);
        }
        Some(len)
    }
}

/// Returns `getaddrinfo`'s error code; any chain written to `res` is freed.
pub fn getaddrinfo_all_null() -> i32 {
    // SAFETY: all-null is a documented input; anything written to `res` is
    // freed before returning.
    unsafe {
        let mut res: *mut AddrInfo = core::ptr::null_mut();
        let ret = dns::getaddrinfo(
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            &mut res,
        );
        if !res.is_null() {
            dns::freeaddrinfo(res);
        }
        ret
    }
}

/// Resolve a numeric IP literal into `(retcode, family of the first result)`;
/// the family is `None` exactly when `retcode != 0`.
pub fn getaddrinfo_numeric(node: &[u8]) -> (i32, Option<i32>) {
    // SAFETY: `node` is NUL-terminated; the result chain is freed before return.
    unsafe {
        let mut res: *mut AddrInfo = core::ptr::null_mut();
        let ret = dns::getaddrinfo(
            node.as_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            &mut res,
        );
        let family = if ret == 0 && !res.is_null() {
            Some((*res).ai_family)
        } else {
            None
        };
        if !res.is_null() {
            dns::freeaddrinfo(res);
        }
        (ret, family)
    }
}

pub fn freeaddrinfo_null_safe() {
    // SAFETY: `freeaddrinfo(null)` is documented as a no-op.
    unsafe { dns::freeaddrinfo(core::ptr::null_mut()) }
}

pub fn inet_addr_invalid_letters() -> u32 {
    // SAFETY: NUL-terminated literal.
    unsafe { addr::inet_addr(b"abc\0".as_ptr()) }
}

pub fn inet_addr_is_none(s: &[u8]) -> bool {
    inet_addr_cstr(s) == INADDR_NONE
}
