//! Safe wrappers over the stdio surface for use from tests.
//!
//! The `snprintf`/`sscanf` wrappers each enumerate one argument shape and
//! contain a single `unsafe` call. [`Stream`] does the same for the `FILE`
//! API: it carries the raw `*mut FILE` that the C entry points take, so a
//! caller can exercise the buffering, direction and locking behaviour without
//! writing `unsafe` itself.

use crate::pal::{Pal, Sys};

use super::{FILE, chars, file, streams};

unsafe extern "C" {
    fn snprintf(buf: *mut u8, n: usize, fmt: *const u8, ...) -> i32;
    fn sscanf(buf: *const u8, fmt: *const u8, ...) -> i32;
}

// ---------------------------------------------------------------------------
// Stream handle
// ---------------------------------------------------------------------------

/// A handle to an open stdio stream.
///
/// `Send`/`Sync` because a `FILE` is exactly the shared object POSIX §2.5.1
/// defines concurrent access to: every operation on it goes through the
/// per-stream lock.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Stream(*mut FILE);

unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

impl Stream {
    fn from_raw(raw: *mut FILE) -> Option<Stream> {
        if raw.is_null() {
            None
        } else {
            Some(Stream(raw))
        }
    }
}

/// The process's standard output stream.
pub fn stdout() -> Stream {
    Stream(streams::stdout_file())
}

/// The process's standard input stream.
pub fn stdin() -> Stream {
    Stream(streams::stdin_file())
}

/// `fopen(path, mode)`. Both slices must be NUL-terminated.
pub fn fopen(path: &[u8], mode: &[u8]) -> Option<Stream> {
    // SAFETY: both slices are live and NUL-terminated by contract.
    Stream::from_raw(unsafe { file::fopen(path.as_ptr(), mode.as_ptr()) })
}

/// `fdopen(fd, mode)`. `mode` must be NUL-terminated.
pub fn fdopen(fd: i32, mode: &[u8]) -> Option<Stream> {
    // SAFETY: `mode` is live and NUL-terminated by contract; `fd` is validated
    // by the callee.
    Stream::from_raw(unsafe { file::fdopen(fd, mode.as_ptr()) })
}

/// `fclose(stream)`. The handle must not be used afterwards.
pub fn fclose(stream: Stream) -> i32 {
    // SAFETY: `stream.0` came from `fopen`/`fdopen` and is still open.
    unsafe { file::fclose(stream.0) }
}

/// `fflush(stream)`.
pub fn fflush(stream: Stream) -> i32 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { file::fflush(stream.0) }
}

/// `fflush(NULL)` — flush every open output stream.
pub fn fflush_all() -> i32 {
    // SAFETY: the null argument is the documented "all streams" request.
    unsafe { file::fflush(core::ptr::null_mut()) }
}

/// `fwrite(data, 1, data.len(), stream)` — returns bytes written.
pub fn fwrite(stream: Stream, data: &[u8]) -> usize {
    // SAFETY: `data` is a live slice of exactly `data.len()` bytes.
    unsafe { file::fwrite(data.as_ptr(), 1, data.len(), stream.0) }
}

/// `fread(out, 1, out.len(), stream)` — returns bytes read.
pub fn fread(stream: Stream, out: &mut [u8]) -> usize {
    // SAFETY: `out` is a live slice of exactly `out.len()` bytes.
    unsafe { file::fread(out.as_mut_ptr(), 1, out.len(), stream.0) }
}

/// `fputc(c, stream)`.
pub fn fputc(c: u8, stream: Stream) -> i32 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { chars::fputc(c as i32, stream.0) }
}

/// `fgetc(stream)`.
pub fn fgetc(stream: Stream) -> i32 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { chars::fgetc(stream.0) }
}

/// `fgets(out, out.len(), stream)` — returns the bytes stored ahead of the
/// terminating NUL, or `None` at end of file.
pub fn fgets(stream: Stream, out: &mut [u8]) -> Option<usize> {
    if out.is_empty() {
        return None;
    }
    let cap = if out.len() > i32::MAX as usize {
        i32::MAX
    } else {
        out.len() as i32
    };
    // SAFETY: `out` is a live slice of at least `cap` bytes.
    if unsafe { chars::fgets(out.as_mut_ptr(), cap, stream.0) }.is_null() {
        return None;
    }
    Some(out.iter().position(|&b| b == 0).unwrap_or(out.len()))
}

/// `fputs(s, stream)`. `s` must be NUL-terminated.
pub fn fputs(stream: Stream, s: &[u8]) -> i32 {
    // SAFETY: `s` is live and NUL-terminated by contract.
    unsafe { chars::fputs(s.as_ptr(), stream.0) }
}

/// `ungetc(c, stream)`.
pub fn ungetc(c: u8, stream: Stream) -> i32 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { chars::ungetc(c as i32, stream.0) }
}

/// `fseek(stream, offset, whence)`.
pub fn fseek(stream: Stream, offset: i64, whence: i32) -> i32 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { file::fseek(stream.0, offset, whence) }
}

/// `ftell(stream)`.
pub fn ftell(stream: Stream) -> i64 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { file::ftell(stream.0) }
}

/// `feof(stream)`.
pub fn feof(stream: Stream) -> i32 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { file::feof(stream.0) }
}

/// `ferror(stream)`.
pub fn ferror(stream: Stream) -> i32 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { file::ferror(stream.0) }
}

/// `clearerr(stream)`.
pub fn clearerr(stream: Stream) {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { file::clearerr(stream.0) }
}

/// `fileno(stream)`.
pub fn fileno(stream: Stream) -> i32 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { file::fileno(stream.0) }
}

/// `setvbuf(stream, NULL, mode, 0)` — mode is `_IOFBF`, `_IOLBF` or `_IONBF`.
pub fn setvbuf_mode(stream: Stream, mode: i32) -> i32 {
    // SAFETY: a null buffer with size 0 is the "keep the internal buffer"
    // request.
    unsafe { file::setvbuf(stream.0, core::ptr::null_mut(), mode, 0) }
}

/// `flockfile(stream)`.
pub fn flockfile(stream: Stream) {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { file::flockfile(stream.0) }
}

/// `funlockfile(stream)`.
pub fn funlockfile(stream: Stream) {
    // SAFETY: the caller holds the lock.
    unsafe { file::funlockfile(stream.0) }
}

/// `ftrylockfile(stream)` — 0 if the lock was taken.
pub fn ftrylockfile(stream: Stream) -> i32 {
    // SAFETY: `stream.0` refers to an open stream.
    unsafe { file::ftrylockfile(stream.0) }
}

// ---------------------------------------------------------------------------
// Raw descriptors
// ---------------------------------------------------------------------------

/// `open(path, O_RDWR|O_CREAT|O_TRUNC, 0666)` — a bare descriptor for
/// `fdopen` to adopt. `path` must be NUL-terminated. Returns -1 on failure.
pub fn open_rw_create(path: &[u8]) -> i32 {
    use crate::ffi::{O_CREAT, O_RDWR, O_TRUNC};
    match Sys::open(path.as_ptr(), O_RDWR | O_CREAT | O_TRUNC, 0o666) {
        Ok(fd) => fd,
        Err(_) => -1,
    }
}

/// `read(fd, out)`, bypassing stdio — the way to observe where a stream left
/// its descriptor. Returns the byte count, or -1.
pub fn read_fd(fd: i32, out: &mut [u8]) -> isize {
    match Sys::read(fd, out.as_mut_ptr(), out.len()) {
        Ok(n) => n as isize,
        Err(_) => -1,
    }
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
