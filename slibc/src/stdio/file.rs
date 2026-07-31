use core::ffi::c_void;
use core::ptr;

use crate::pal::Pal;

use super::{
    _IOFBF, _IOLBF, _IONBF, BufferMode, EOF, FILE, FILE_FLAG_EOF, FILE_FLAG_ERR, FILE_FLAG_HEAP,
    FILE_FLAG_OWNED_FD, FILE_FLAG_READABLE, FILE_FLAG_READING, FILE_FLAG_WRITABLE,
    FILE_FLAG_WRITING, SEEK_CUR, SEEK_END, SEEK_SET, WalkMode, registry,
};
use crate::ffi::{O_APPEND, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
use crate::mem::malloc;
use crate::pal::Sys;

fn parse_mode(mode: *const u8) -> Option<(i32, u32)> {
    if mode.is_null() {
        return None;
    }
    unsafe {
        let first = *mode;
        let second = *mode.add(1);
        let has_plus = second == b'+' || (second != 0 && *mode.add(2) == b'+');

        match (first, has_plus) {
            (b'r', false) => Some((O_RDONLY, FILE_FLAG_READABLE)),
            (b'r', true) => Some((O_RDWR, FILE_FLAG_READABLE | FILE_FLAG_WRITABLE)),
            (b'w', false) => Some((O_WRONLY | O_CREAT | O_TRUNC, FILE_FLAG_WRITABLE)),
            (b'w', true) => Some((
                O_RDWR | O_CREAT | O_TRUNC,
                FILE_FLAG_READABLE | FILE_FLAG_WRITABLE,
            )),
            (b'a', false) => Some((O_WRONLY | O_CREAT | O_APPEND, FILE_FLAG_WRITABLE)),
            (b'a', true) => Some((
                O_RDWR | O_CREAT | O_APPEND,
                FILE_FLAG_READABLE | FILE_FLAG_WRITABLE,
            )),
            _ => None,
        }
    }
}

/// Allocate a `FILE` for `fd` and publish it on the open-stream list.
///
/// The allocation happens before the list lock is taken: no stdio lock is ever
/// held across `malloc`.
///
/// # Safety
/// `fd` must be open and must not already back another stream.
unsafe fn new_stream(fd: i32, fflags: u32) -> *mut FILE {
    let raw = malloc::alloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if raw.is_null() {
        return ptr::null_mut();
    }

    // C11 §7.21.3: a stream refers to an interactive device or it does not,
    // and only the former is line-buffered by default.
    let buf_mode = if crate::io::shim::isatty(fd) != 0 {
        BufferMode::Line
    } else {
        BufferMode::Full
    };

    FILE::init_at(raw, fd, buf_mode, fflags | FILE_FLAG_HEAP);
    registry::link(raw);
    raw
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fopen(path: *const u8, mode: *const u8) -> *mut FILE {
    let (oflags, fflags) = match parse_mode(mode) {
        Some(v) => v,
        None => return ptr::null_mut(),
    };

    let fd = match Sys::open(path, oflags, 0o666) {
        Ok(fd) => fd,
        Err(_) => return ptr::null_mut(),
    };

    let stream = new_stream(fd, fflags | FILE_FLAG_OWNED_FD);
    if stream.is_null() {
        let _ = Sys::close(fd);
    }
    stream
}

/// Associate a stream with an already-open descriptor.
///
/// POSIX: `fclose` on the result closes `fd`, and an append mode positions the
/// descriptor at end-of-file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fdopen(fd: i32, mode: *const u8) -> *mut FILE {
    if fd < 0 {
        return ptr::null_mut();
    }
    let (oflags, fflags) = match parse_mode(mode) {
        Some(v) => v,
        None => return ptr::null_mut(),
    };

    if oflags & O_APPEND != 0 {
        let _ = Sys::lseek(fd, 0, SEEK_END);
    }

    new_stream(fd, fflags | FILE_FLAG_OWNED_FD)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fclose(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }

    // Unlink first: a concurrent `fflush(NULL)` must never reach a node that is
    // about to be freed.
    registry::unlink(stream);

    (*stream).lock.lock();
    let f = &mut *stream;
    let mut ret = 0i32;

    if f.flags & FILE_FLAG_WRITING != 0 && f.flush_write_buf() == EOF {
        ret = EOF;
    }
    if f.flags & FILE_FLAG_READING != 0 {
        f.discard_read_ahead();
    }

    if f.flags & FILE_FLAG_OWNED_FD != 0 && Sys::close(f.fd).is_err() {
        ret = EOF;
    }

    let heap = f.flags & FILE_FLAG_HEAP != 0;
    (*stream).lock.unlock();

    if heap {
        malloc::dealloc(stream as *mut c_void);
    }

    ret
}

unsafe fn fread_core(ptr: *mut u8, size: usize, nmemb: usize, stream: *mut FILE) -> usize {
    let f = &mut *stream;
    if f.flags & FILE_FLAG_READABLE == 0 || !f.to_read() {
        f.flags |= FILE_FLAG_ERR;
        return 0;
    }

    let total = size.saturating_mul(nmemb);
    let mut done = 0usize;

    while done < total {
        // Check ungetc push-back first
        if f.ungot >= 0 {
            *ptr.add(done) = f.ungot as u8;
            f.ungot = -1;
            f.flags &= !FILE_FLAG_EOF;
            done += 1;
            continue;
        }

        if f.buf_pos >= f.buf_len {
            if f.fill_read_buf() == EOF {
                break;
            }
        }

        let avail = f.buf_len - f.buf_pos;
        let want = total - done;
        let chunk = if avail < want { avail } else { want };

        ptr::copy_nonoverlapping(f.buf[f.buf_pos..].as_ptr(), ptr.add(done), chunk);
        f.buf_pos += chunk;
        done += chunk;
    }

    done / size
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fread(
    ptr: *mut u8,
    size: usize,
    nmemb: usize,
    stream: *mut FILE,
) -> usize {
    if ptr.is_null() || stream.is_null() || size == 0 || nmemb == 0 {
        return 0;
    }
    (*stream).lock.lock();
    let done = fread_core(ptr, size, nmemb, stream);
    (*stream).lock.unlock();
    done
}

/// `fread` without taking the stream lock (POSIX §2.5.1 unlocked variant).
///
/// # Safety
/// The caller holds the stream lock, or the stream is thread-private.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fread_unlocked(
    ptr: *mut u8,
    size: usize,
    nmemb: usize,
    stream: *mut FILE,
) -> usize {
    if ptr.is_null() || stream.is_null() || size == 0 || nmemb == 0 {
        return 0;
    }
    fread_core(ptr, size, nmemb, stream)
}

unsafe fn fwrite_core(ptr: *const u8, size: usize, nmemb: usize, stream: *mut FILE) -> usize {
    let f = &mut *stream;
    if f.flags & FILE_FLAG_WRITABLE == 0 || !f.to_write() {
        f.flags |= FILE_FLAG_ERR;
        return 0;
    }

    let total = size.saturating_mul(nmemb);

    match f.mode {
        BufferMode::None => {
            // Unbuffered: write directly
            let mut done = 0usize;
            while done < total {
                match Sys::write(f.fd, ptr.add(done), total - done) {
                    Ok(n) => {
                        if n == 0 {
                            f.flags |= FILE_FLAG_ERR;
                            break;
                        }
                        done += n;
                    }
                    Err(_) => {
                        f.flags |= FILE_FLAG_ERR;
                        break;
                    }
                }
            }
            done / size
        }
        BufferMode::Full => {
            let mut done = 0usize;
            while done < total {
                let space = super::BUFSIZ - f.buf_pos;
                let want = total - done;
                let chunk = if space < want { space } else { want };

                ptr::copy_nonoverlapping(ptr.add(done), f.buf[f.buf_pos..].as_mut_ptr(), chunk);
                f.buf_pos += chunk;
                done += chunk;

                if f.buf_pos >= super::BUFSIZ {
                    if f.flush_write_buf() == EOF {
                        break;
                    }
                }
            }
            done / size
        }
        BufferMode::Line => {
            let mut done = 0usize;
            while done < total {
                let b = *ptr.add(done);
                if f.buf_pos >= super::BUFSIZ {
                    if f.flush_write_buf() == EOF {
                        break;
                    }
                }
                f.buf[f.buf_pos] = b;
                f.buf_pos += 1;
                done += 1;

                if b == b'\n' {
                    if f.flush_write_buf() == EOF {
                        break;
                    }
                }
            }
            done / size
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fwrite(
    ptr: *const u8,
    size: usize,
    nmemb: usize,
    stream: *mut FILE,
) -> usize {
    if ptr.is_null() || stream.is_null() || size == 0 || nmemb == 0 {
        return 0;
    }
    (*stream).lock.lock();
    let done = fwrite_core(ptr, size, nmemb, stream);
    (*stream).lock.unlock();
    done
}

/// `fwrite` without taking the stream lock (POSIX §2.5.1 unlocked variant).
///
/// # Safety
/// The caller holds the stream lock, or the stream is thread-private.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fwrite_unlocked(
    ptr: *const u8,
    size: usize,
    nmemb: usize,
    stream: *mut FILE,
) -> usize {
    if ptr.is_null() || stream.is_null() || size == 0 || nmemb == 0 {
        return 0;
    }
    fwrite_core(ptr, size, nmemb, stream)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fseek(stream: *mut FILE, offset: i64, whence: i32) -> i32 {
    if stream.is_null() {
        return -1;
    }

    (*stream).lock.lock();
    let f = &mut *stream;

    if f.flags & FILE_FLAG_WRITING != 0 {
        f.flush_write_buf();
    }

    // A `SEEK_CUR` offset is relative to the stream position, but the fd sits
    // `read_ahead_len()` bytes past it while the buffer holds read-ahead.
    let effective = if whence == SEEK_CUR && f.flags & FILE_FLAG_READING != 0 {
        offset - f.read_ahead_len()
    } else {
        offset
    };

    f.buf_pos = 0;
    f.buf_len = 0;
    f.ungot = -1;
    // C11 §7.21.9.2: a successful `fseek` clears the end-of-file indicator and
    // undoes `ungetc`. It says nothing about the error indicator.
    f.flags &= !(FILE_FLAG_EOF | FILE_FLAG_READING | FILE_FLAG_WRITING);

    let ret = match Sys::lseek(f.fd, effective, whence) {
        Ok(_) => 0,
        Err(_) => -1,
    };
    (*stream).lock.unlock();
    ret
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ftell(stream: *mut FILE) -> i64 {
    if stream.is_null() {
        return -1;
    }
    (*stream).lock.lock();
    let f = &mut *stream;
    let ret = match Sys::lseek(f.fd, 0, SEEK_CUR) {
        // The fd offset trails the stream position by the bytes buffered for
        // output, and leads it by the bytes buffered from input.
        Ok(pos) => {
            if f.flags & FILE_FLAG_WRITING != 0 {
                pos + f.buf_pos as i64
            } else if f.flags & FILE_FLAG_READING != 0 {
                pos - f.read_ahead_len()
            } else {
                pos
            }
        }
        Err(_) => -1,
    };
    (*stream).lock.unlock();
    ret
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rewind(stream: *mut FILE) {
    if !stream.is_null() {
        // The stream lock is recursive, so the seek and the error-flag clear
        // that C11 §7.21.9.5 specifies together are observed together.
        (*stream).lock.lock();
        fseek(stream, 0, SEEK_SET);
        (*stream).flags &= !FILE_FLAG_ERR;
        (*stream).lock.unlock();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fflush(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return registry::flush_all(WalkMode::Blocking);
    }

    (*stream).lock.lock();
    let f = &mut *stream;
    let ret = if f.flags & FILE_FLAG_WRITING != 0 {
        f.flush_write_buf()
    } else if f.flags & FILE_FLAG_READING != 0 {
        // POSIX: on an input stream, `fflush` gives back the read-ahead by
        // repositioning the descriptor to the stream position and dropping it.
        f.discard_read_ahead();
        0
    } else {
        0
    };
    (*stream).lock.unlock();
    ret
}

/// `feof` deliberately does not take the stream lock: the answer is a single
/// aligned load, and POSIX §2.5.1's locking would only make a stale answer
/// arrive later, not make it fresh.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn feof(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return 0;
    }
    if (*stream).flags & FILE_FLAG_EOF != 0 {
        1
    } else {
        0
    }
}

/// Unlocked for the same reason as [`feof`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ferror(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return 0;
    }
    if (*stream).flags & FILE_FLAG_ERR != 0 {
        1
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn clearerr(stream: *mut FILE) {
    if !stream.is_null() {
        (*stream).lock.lock();
        (*stream).flags &= !(FILE_FLAG_EOF | FILE_FLAG_ERR);
        (*stream).lock.unlock();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setvbuf(stream: *mut FILE, _buf: *mut u8, mode: i32, _size: usize) -> i32 {
    if stream.is_null() {
        return -1;
    }
    let new_mode = match mode {
        _IOFBF => BufferMode::Full,
        _IOLBF => BufferMode::Line,
        _IONBF => BufferMode::None,
        _ => return -1,
    };
    (*stream).lock.lock();
    (*stream).mode = new_mode;
    (*stream).lock.unlock();
    0
}

/// Unlocked for the same reason as [`feof`] — the descriptor never changes for
/// the life of the stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fileno(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return -1;
    }
    (*stream).fd
}

// ---------------------------------------------------------------------------
// Explicit stream locking (POSIX §2.5.2)
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn flockfile(stream: *mut FILE) {
    if !stream.is_null() {
        (*stream).lock.lock();
    }
}

/// Returns 0 if the lock was acquired, non-zero otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ftrylockfile(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return -1;
    }
    if (*stream).lock.try_lock() { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn funlockfile(stream: *mut FILE) {
    if !stream.is_null() {
        (*stream).lock.unlock();
    }
}
