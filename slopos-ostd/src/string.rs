use core::ffi::{CStr, c_char};

/// Convert a C string pointer to a Rust `&str`.
///
/// Returns `"<null>"` for null pointers and `"<invalid utf-8>"` for
/// non-UTF-8 data. Intended for FFI boundaries where a `*const c_char`
/// arrives from C-style APIs (bootloader, IST stack names, etc.).
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

/// Best-effort safe variant of [`cstr_to_str`]. Returns `"<null>"`
/// for null pointers; for non-null pointers performs the same
/// `CStr::from_ptr` walk as `cstr_to_str` and returns
/// `"<invalid utf-8>"` on bad UTF-8.
///
/// "Safe" here means the function is callable from safe Rust — the
/// underlying contract (the pointer must reference a NUL-terminated
/// string for some bounded length) is the caller's responsibility, but
/// our consumers are exclusively boot-time diagnostic paths reading
/// from static C strings published by IST glue / Limine, where the
/// invariant is well-known.
#[inline]
pub fn cstr_to_str_lossy(ptr: *const c_char) -> &'static str {
    // SAFETY: see doc-comment — production callers are kdiag/IST
    // diagnostics reading from static-lifetime NUL-terminated strings.
    unsafe { cstr_to_str(ptr) }
}

/// Extract a NUL-padded byte array as a `&str`.
///
/// Scans for the first NUL byte (or end of slice) and interprets the
/// prefix as UTF-8. Returns `"<invalid>"` if the bytes are not valid
/// UTF-8, or `""` if the buffer starts with NUL / is empty.
#[inline]
pub fn bytes_as_str(buf: &[u8]) -> &str {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..len]).unwrap_or("<invalid>")
}
