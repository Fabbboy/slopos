use core::ffi::{CStr, c_char};

/// Convert a C string pointer to a Rust `&str`.
///
/// Returns `"<null>"` for null pointers and `"<invalid utf-8>"` for
/// non-UTF-8 data.
///
/// # Safety
///
/// The pointer must be valid and point to a NUL-terminated string,
/// or be null.
#[inline]
pub unsafe fn cstr_to_str(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "<null>";
    }
    unsafe { CStr::from_ptr(ptr).to_str().unwrap_or("<invalid utf-8>") }
}

/// Best-effort safe variant of [`cstr_to_str`], with the same results.
///
/// Callable from safe Rust only because every consumer is a boot-time
/// diagnostic path reading static NUL-terminated C strings published by IST
/// glue / Limine; the pointer contract is still the caller's.
#[inline]
pub fn cstr_to_str_lossy(ptr: *const c_char) -> &'static str {
    // SAFETY: see doc-comment — production callers are kdiag/IST
    // diagnostics reading from static-lifetime NUL-terminated strings.
    unsafe { cstr_to_str(ptr) }
}

/// Extract a NUL-padded byte array as a `&str`.
///
/// Truncates at the first NUL; returns `"<invalid>"` if the prefix is not
/// valid UTF-8.
#[inline]
pub fn bytes_as_str(buf: &[u8]) -> &str {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..len]).unwrap_or("<invalid>")
}
