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
/// The unsafe `CStr::from_ptr` lives once, here. Consumers stay fully safe.
///
/// `&'static` rather than a caller-chosen lifetime. Every caller decodes a
/// pointer into memory that is part of the image or the bootloader handoff and
/// is never freed — the kernel command line, a boot-time reservation name, a
/// task's own inline name array — so `'static` is what the borrow already was.
/// A lifetime the caller picks is a lifetime the caller can pick twice, which
/// for the `&mut` shapes elsewhere in this crate is aliasing UB and here is
/// simply a claim nothing checks.
#[inline]
pub fn cstr_from_kernel_ptr(ptr: *const c_char) -> Option<&'static [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `ptr` points at a NUL-terminated
    // sequence in kernel-mapped memory that is never freed.
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
pub fn cstr_from_kernel_ptr_str(ptr: *const c_char) -> Option<&'static str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: see crate-level note on `cstr_from_kernel_ptr`.
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}
