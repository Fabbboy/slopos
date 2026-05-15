//! Safe wrappers over `net::addr` and `net::dns` for use from tests.

use super::addr::{self, INADDR_NONE};
use super::dns::{self, AddrInfo};

/// Parse a NUL-terminated dotted-quad. Returns `INADDR_NONE` for
/// invalid input (matching `inet_addr` semantics).
pub fn inet_addr_cstr(s: &[u8]) -> u32 {
    // SAFETY: caller provides a NUL-terminated byte slice; `inet_addr`
    // reads bytes until the NUL and never beyond.
    unsafe { addr::inet_addr(s.as_ptr()) }
}

/// Calls `inet_addr` with a null pointer. Exercises the documented
/// null-input path; expected to return `INADDR_NONE`.
pub fn inet_addr_null() -> u32 {
    // SAFETY: `inet_addr` documents `cp == null` as a defined input
    // that returns `INADDR_NONE`.
    unsafe { addr::inet_addr(core::ptr::null()) }
}

/// Return the length of `inet_ntoa(addr)`'s output up to (excluding) the
/// terminating NUL. `None` if the pointer is null.
pub fn inet_ntoa_len(addr: u32) -> Option<usize> {
    // SAFETY: `inet_ntoa` returns a pointer into a static buffer (per
    // libc convention), valid until the next call. We read bytes until
    // the NUL without exceeding the buffer.
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

/// Wrap `getaddrinfo(null, null, null, &mut res)` — the all-null
/// invocation. Returns the error code; `res` is dropped if it was set.
pub fn getaddrinfo_all_null() -> i32 {
    // SAFETY: all-null is the documented "no node, no service, no hints"
    // input. `getaddrinfo` won't write through `res` unless it succeeds;
    // we free anything it does write before returning.
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

/// Resolve a numeric IP literal. Returns `(retcode, first_family)`
/// where `first_family` is `Some(af)` on success or `None` on error
/// (in which case `retcode != 0`).
pub fn getaddrinfo_numeric(node: &[u8]) -> (i32, Option<i32>) {
    // SAFETY: `node` is a NUL-terminated byte slice; service/hints null
    // is documented. Result chain is freed before return.
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

/// Exercise the null-pointer accept path of `freeaddrinfo`.
pub fn freeaddrinfo_null_safe() {
    // SAFETY: `freeaddrinfo(null)` is documented as a no-op.
    unsafe { dns::freeaddrinfo(core::ptr::null_mut()) }
}

pub fn inet_addr_invalid_letters() -> u32 {
    // SAFETY: caller-provided NUL-terminated literal.
    unsafe { addr::inet_addr(b"abc\0".as_ptr()) }
}

/// Convenience wrapper checking `INADDR_NONE` semantics for an empty
/// NUL-terminated input.
pub fn inet_addr_is_none(s: &[u8]) -> bool {
    inet_addr_cstr(s) == INADDR_NONE
}
