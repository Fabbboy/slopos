//! Safe wrapper for kernel-side C-string pointer parsing.
//!
//! Replaces the per-call-site `unsafe { CStr::from_ptr(ptr).to_bytes() }`
//! pattern with one centralised null-check + reborrow.

use core::ffi::{CStr, c_char};

/// Null-checked, length-bounded read of a C-string from a kernel
/// pointer.
///
/// Returns the byte slice up to (but not including) the terminating
/// NUL on success, or `None` if the pointer is null.
///
/// SAFETY (caller, weakly): `ptr` must either be null or point to a
/// NUL-terminated byte sequence inside kernel-mapped memory. Most
/// kernel-half call sites get such a pointer from a `&'static [u8]`
/// literal or from a structurally validated bootloader handoff (e.g.
/// the IST-stack name lookup); for those the SAFETY obligation is
/// trivially discharged by the call-site context.
///
/// The unsafe `CStr::from_ptr` lives once, here. Consumers stay fully
/// safe.
#[inline]
pub fn cstr_from_kernel_ptr<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `ptr` points at a NUL-terminated
    // sequence in kernel-mapped memory; `CStr::from_ptr` walks until
    // the NUL and returns a borrow tied to the caller's lifetime
    // anchor.
    Some(unsafe { CStr::from_ptr(ptr) }.to_bytes())
}

/// Null-checked decode of a kernel C-string pointer as UTF-8 `&str`.
///
/// Returns `None` if the pointer is null *or* the bytes between the
/// pointer and the terminating NUL are not valid UTF-8. The unsafe
/// `CStr::from_ptr` walk is centralised here; consumers stay safe.
///
/// SAFETY (caller, weakly): same as [`cstr_from_kernel_ptr`] — `ptr`
/// must be either null or NUL-terminated inside kernel-mapped memory.
#[inline]
pub fn cstr_from_kernel_ptr_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: see crate-level note on `cstr_from_kernel_ptr`.
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}
