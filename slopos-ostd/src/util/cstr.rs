//! Safe wrapper for kernel-side C-string pointer parsing: the kernel's one
//! null-check + `CStr::from_ptr` reborrow.

use core::ffi::{CStr, c_char};

/// Bytes up to (not including) the terminating NUL, or `None` if `ptr` is
/// null.
///
/// SAFETY (caller, weakly): `ptr` must either be null or point to a
/// NUL-terminated byte sequence inside kernel-mapped memory.
///
/// `&'static` rather than a caller-chosen lifetime: every caller decodes into
/// image or bootloader-handoff memory that is never freed, and a lifetime the
/// caller picks is a claim nothing checks.
#[inline]
pub fn cstr_from_kernel_ptr(ptr: *const c_char) -> Option<&'static [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `ptr` points at a NUL-terminated
    // sequence in kernel-mapped memory that is never freed.
    Some(unsafe { CStr::from_ptr(ptr) }.to_bytes())
}

/// Decode of a kernel C-string pointer as UTF-8, `None` if `ptr` is null or
/// the bytes are not valid UTF-8.
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
