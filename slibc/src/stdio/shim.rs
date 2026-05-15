//! Safe wrappers over the `c_variadic` `snprintf`/`sscanf` for use from
//! tests. Each wrapper enumerates one argument shape and contains a
//! single `unsafe { snprintf(...) }` / `unsafe { sscanf(...) }` call.

unsafe extern "C" {
    fn snprintf(buf: *mut u8, n: usize, fmt: *const u8, ...) -> i32;
    fn sscanf(buf: *const u8, fmt: *const u8, ...) -> i32;
}

/// `snprintf(buf, fmt)` — fmt-only, no further args.
pub fn snprintf_fmt_only(buf: &mut [u8], fmt: &[u8]) -> i32 {
    // SAFETY: `buf` and `fmt` are live slices; `fmt` is caller-supplied
    // NUL-terminated; no extra args expected.
    unsafe { snprintf(buf.as_mut_ptr(), buf.len(), fmt.as_ptr()) }
}

/// `snprintf(buf, fmt, i32)`.
pub fn snprintf_d(buf: &mut [u8], fmt: &[u8], v: i32) -> i32 {
    // SAFETY: see `snprintf_fmt_only`; one i32 variadic.
    unsafe { snprintf(buf.as_mut_ptr(), buf.len(), fmt.as_ptr(), v) }
}

/// `snprintf(buf, fmt, u32)`.
pub fn snprintf_u(buf: &mut [u8], fmt: &[u8], v: u32) -> i32 {
    // SAFETY: see `snprintf_fmt_only`; one u32 variadic.
    unsafe { snprintf(buf.as_mut_ptr(), buf.len(), fmt.as_ptr(), v) }
}

/// `snprintf(buf, fmt, *const u8)` for `%s`.
pub fn snprintf_s(buf: &mut [u8], fmt: &[u8], s: *const u8) -> i32 {
    // SAFETY: caller supplies the string pointer; passing null is the
    // documented `(null)` path for `%s`.
    unsafe { snprintf(buf.as_mut_ptr(), buf.len(), fmt.as_ptr(), s) }
}

/// `snprintf(buf, fmt, *const u8, i32)` for `%s=%d`-style.
pub fn snprintf_sd(buf: &mut [u8], fmt: &[u8], s: *const u8, v: i32) -> i32 {
    // SAFETY: caller supplies the string pointer; the i32 is a plain
    // value.
    unsafe { snprintf(buf.as_mut_ptr(), buf.len(), fmt.as_ptr(), s, v) }
}

/// `snprintf(buf_with_capped_len, fmt, i32)` — used for truncation
/// tests that pass an explicit `n` smaller than the buffer's true
/// length.
pub fn snprintf_d_with_cap(buf: &mut [u8], cap: usize, fmt: &[u8], v: i32) -> i32 {
    // SAFETY: `buf` outlives the call; `cap <= buf.len()` is the
    // caller's responsibility (asserted here defensively).
    assert!(cap <= buf.len());
    unsafe { snprintf(buf.as_mut_ptr(), cap, fmt.as_ptr(), v) }
}

/// `sscanf(buf, fmt, &mut i32)`.
pub fn sscanf_d(buf: &[u8], fmt: &[u8], out: &mut i32) -> i32 {
    // SAFETY: `out` is a live `&mut i32`; `buf`/`fmt` are
    // NUL-terminated byte slices.
    unsafe { sscanf(buf.as_ptr(), fmt.as_ptr(), out as *mut i32) }
}

/// `sscanf(buf, fmt, &mut [u8; _])` for `%s`. Caller supplies the
/// receive buffer; sscanf writes up to a NUL.
pub fn sscanf_s(buf: &[u8], fmt: &[u8], out: &mut [u8]) -> i32 {
    // SAFETY: `out` is a live `&mut [u8]`; sscanf writes through its
    // pointer, bounded by the format directive.
    unsafe { sscanf(buf.as_ptr(), fmt.as_ptr(), out.as_mut_ptr()) }
}

/// `sscanf(buf, fmt, &mut i32, &mut i32)`.
pub fn sscanf_dd(buf: &[u8], fmt: &[u8], a: &mut i32, b: &mut i32) -> i32 {
    // SAFETY: `a` and `b` are live `&mut i32`s.
    unsafe { sscanf(buf.as_ptr(), fmt.as_ptr(), a as *mut i32, b as *mut i32) }
}
