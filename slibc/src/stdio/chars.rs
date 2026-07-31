//! Character and line primitives.
//!
//! Each one is an unlocked core plus a wrapper that takes the stream lock, so
//! callers that already hold the stream (POSIX §2.5.2 `flockfile`, and the
//! `printf`/`scanf` families internally) pay for one acquisition per call
//! rather than one per byte.

use crate::pal::Pal;

use super::{
    BufferMode, EOF, FILE, FILE_FLAG_EOF, FILE_FLAG_ERR, FILE_FLAG_READABLE, FILE_FLAG_WRITABLE,
};

// ---------------------------------------------------------------------------
// Unlocked cores
// ---------------------------------------------------------------------------

/// # Safety
/// The caller holds the stream lock, or the stream is thread-private.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fgetc_unlocked(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }

    let f = &mut *stream;
    if f.flags & FILE_FLAG_READABLE == 0 || !f.to_read() {
        f.flags |= FILE_FLAG_ERR;
        return EOF;
    }

    // Check ungetc push-back first
    if f.ungot >= 0 {
        let c = f.ungot;
        f.ungot = -1;
        f.flags &= !FILE_FLAG_EOF;
        return c;
    }

    if f.buf_pos >= f.buf_len {
        if f.fill_read_buf() == EOF {
            return EOF;
        }
    }

    let c = f.buf[f.buf_pos] as i32;
    f.buf_pos += 1;
    c
}

/// # Safety
/// The caller holds the stream lock, or the stream is thread-private.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fputc_unlocked(c: i32, stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }

    let f = &mut *stream;
    if f.flags & FILE_FLAG_WRITABLE == 0 || !f.to_write() {
        f.flags |= FILE_FLAG_ERR;
        return EOF;
    }

    let byte = c as u8;

    match f.mode {
        BufferMode::None => {
            let buf = [byte];
            match crate::pal::Sys::write(f.fd, buf.as_ptr(), 1) {
                Ok(1) => c,
                _ => {
                    f.flags |= FILE_FLAG_ERR;
                    EOF
                }
            }
        }
        BufferMode::Full => {
            if f.buf_pos >= super::BUFSIZ {
                if f.flush_write_buf() == EOF {
                    return EOF;
                }
            }
            f.buf[f.buf_pos] = byte;
            f.buf_pos += 1;
            c
        }
        BufferMode::Line => {
            if f.buf_pos >= super::BUFSIZ {
                if f.flush_write_buf() == EOF {
                    return EOF;
                }
            }
            f.buf[f.buf_pos] = byte;
            f.buf_pos += 1;
            if byte == b'\n' {
                if f.flush_write_buf() == EOF {
                    return EOF;
                }
            }
            c
        }
    }
}

/// # Safety
/// The caller holds the stream lock, or the stream is thread-private.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fgets_unlocked(s: *mut u8, n: i32, stream: *mut FILE) -> *mut u8 {
    if s.is_null() || stream.is_null() || n <= 0 {
        return core::ptr::null_mut();
    }

    let max = (n - 1) as usize;
    let mut i = 0usize;

    while i < max {
        let c = fgetc_unlocked(stream);
        if c == EOF {
            if i == 0 {
                return core::ptr::null_mut();
            }
            break;
        }
        *s.add(i) = c as u8;
        i += 1;
        if c as u8 == b'\n' {
            break;
        }
    }

    *s.add(i) = 0;
    s
}

/// # Safety
/// The caller holds the stream lock, or the stream is thread-private.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fputs_unlocked(s: *const u8, stream: *mut FILE) -> i32 {
    if s.is_null() || stream.is_null() {
        return EOF;
    }

    let mut p = s;
    while *p != 0 {
        if fputc_unlocked(*p as i32, stream) == EOF {
            return EOF;
        }
        p = p.add(1);
    }
    0
}

/// # Safety
/// The caller holds the stream lock, or the stream is thread-private.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ungetc_unlocked(c: i32, stream: *mut FILE) -> i32 {
    if stream.is_null() || c == EOF {
        return EOF;
    }

    let f = &mut *stream;
    if f.ungot >= 0 {
        // Only one byte of push-back is supported
        return EOF;
    }

    f.ungot = c & 0xFF;
    f.flags &= !FILE_FLAG_EOF;
    f.ungot
}

// ---------------------------------------------------------------------------
// Locking wrappers
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fgetc(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }
    (*stream).lock.lock();
    let c = fgetc_unlocked(stream);
    (*stream).lock.unlock();
    c
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fputc(c: i32, stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }
    (*stream).lock.lock();
    let r = fputc_unlocked(c, stream);
    (*stream).lock.unlock();
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fgets(s: *mut u8, n: i32, stream: *mut FILE) -> *mut u8 {
    if stream.is_null() {
        return core::ptr::null_mut();
    }
    (*stream).lock.lock();
    let r = fgets_unlocked(s, n, stream);
    (*stream).lock.unlock();
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fputs(s: *const u8, stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }
    (*stream).lock.lock();
    let r = fputs_unlocked(s, stream);
    (*stream).lock.unlock();
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ungetc(c: i32, stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }
    (*stream).lock.lock();
    let r = ungetc_unlocked(c, stream);
    (*stream).lock.unlock();
    r
}

// ---------------------------------------------------------------------------
// getc / putc and the standard-stream shorthands
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getc(stream: *mut FILE) -> i32 {
    fgetc(stream)
}

/// # Safety
/// The caller holds the stream lock, or the stream is thread-private.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getc_unlocked(stream: *mut FILE) -> i32 {
    fgetc_unlocked(stream)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn putc(c: i32, stream: *mut FILE) -> i32 {
    fputc(c, stream)
}

/// # Safety
/// The caller holds the stream lock, or the stream is thread-private.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn putc_unlocked(c: i32, stream: *mut FILE) -> i32 {
    fputc_unlocked(c, stream)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getchar() -> i32 {
    fgetc(super::streams::stdin_file())
}

/// # Safety
/// The caller holds stdin's lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getchar_unlocked() -> i32 {
    fgetc_unlocked(super::streams::stdin_file())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn putchar(c: i32) -> i32 {
    fputc(c, super::streams::stdout_file())
}

/// # Safety
/// The caller holds stdout's lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn putchar_unlocked(c: i32) -> i32 {
    fputc_unlocked(c, super::streams::stdout_file())
}

/// `puts` writes the string and a newline as one unit: POSIX §2.5.1 makes the
/// whole call atomic with respect to other stdio calls on the same stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn puts(s: *const u8) -> i32 {
    if s.is_null() {
        return EOF;
    }
    let out = super::streams::stdout_file();
    (*out).lock.lock();
    let mut ret = 0i32;
    if fputs_unlocked(s, out) == EOF || fputc_unlocked(b'\n' as i32, out) == EOF {
        ret = EOF;
    }
    (*out).lock.unlock();
    ret
}
