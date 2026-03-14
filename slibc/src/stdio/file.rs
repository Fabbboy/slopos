use core::ffi::c_void;
use core::ptr;

use crate::pal::Pal;

use super::{
    _IOFBF, _IOLBF, _IONBF, BufferMode, EOF, FILE, FILE_FLAG_EOF, FILE_FLAG_ERR,
    FILE_FLAG_OWNED_FD, FILE_FLAG_READABLE, FILE_FLAG_WRITABLE, SEEK_CUR, SEEK_SET,
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

    let buf_mode = if fflags & FILE_FLAG_WRITABLE != 0 {
        BufferMode::Full
    } else {
        BufferMode::Full
    };

    let ptr = malloc::alloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if ptr.is_null() {
        let _ = Sys::close(fd);
        return ptr::null_mut();
    }

    ptr::write(ptr, FILE::new(fd, buf_mode, fflags | FILE_FLAG_OWNED_FD));
    ptr
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fclose(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }

    let f = &mut *stream;
    let mut ret = 0i32;

    if f.flags & FILE_FLAG_WRITABLE != 0 {
        if f.flush_write_buf() == EOF {
            ret = EOF;
        }
    }

    if f.flags & FILE_FLAG_OWNED_FD != 0 {
        if Sys::close(f.fd).is_err() {
            ret = EOF;
        }
    }

    let is_static = is_standard_stream(stream);
    if !is_static {
        malloc::dealloc(stream as *mut c_void);
    }

    ret
}

fn is_standard_stream(stream: *mut FILE) -> bool {
    unsafe {
        stream == super::streams::stdin_file()
            || stream == super::streams::stdout_file()
            || stream == super::streams::stderr_file()
    }
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

    let f = &mut *stream;
    if f.flags & FILE_FLAG_READABLE == 0 {
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
pub unsafe extern "C" fn fwrite(
    ptr: *const u8,
    size: usize,
    nmemb: usize,
    stream: *mut FILE,
) -> usize {
    if ptr.is_null() || stream.is_null() || size == 0 || nmemb == 0 {
        return 0;
    }

    let f = &mut *stream;
    if f.flags & FILE_FLAG_WRITABLE == 0 {
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
pub unsafe extern "C" fn fseek(stream: *mut FILE, offset: i64, whence: i32) -> i32 {
    if stream.is_null() {
        return -1;
    }

    let f = &mut *stream;

    if f.flags & FILE_FLAG_WRITABLE != 0 {
        f.flush_write_buf();
    }

    // Discard read buffer
    f.buf_pos = 0;
    f.buf_len = 0;
    f.ungot = -1;
    f.flags &= !(FILE_FLAG_EOF | FILE_FLAG_ERR);

    match Sys::lseek(f.fd, offset, whence) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ftell(stream: *mut FILE) -> i64 {
    if stream.is_null() {
        return -1;
    }
    let f = &mut *stream;
    match Sys::lseek(f.fd, 0, SEEK_CUR) {
        Ok(pos) => {
            // Adjust for buffered but unread data
            let buffered_unread = f.buf_len as i64 - f.buf_pos as i64;
            let ungot_adj = if f.ungot >= 0 { 1i64 } else { 0i64 };
            pos - buffered_unread - ungot_adj
        }
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rewind(stream: *mut FILE) {
    if !stream.is_null() {
        fseek(stream, 0, SEEK_SET);
        (*stream).flags &= !FILE_FLAG_ERR;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fflush(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        // Flush all standard streams
        let mut ret = 0i32;
        let out = super::streams::stdout_file();
        if (*out).flags & FILE_FLAG_WRITABLE != 0 && (*out).flush_write_buf() == EOF {
            ret = EOF;
        }
        let err = super::streams::stderr_file();
        if (*err).flags & FILE_FLAG_WRITABLE != 0 && (*err).flush_write_buf() == EOF {
            ret = EOF;
        }
        return ret;
    }

    let f = &mut *stream;
    if f.flags & FILE_FLAG_WRITABLE != 0 {
        f.flush_write_buf()
    } else {
        0
    }
}

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
        (*stream).flags &= !(FILE_FLAG_EOF | FILE_FLAG_ERR);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setvbuf(stream: *mut FILE, _buf: *mut u8, mode: i32, _size: usize) -> i32 {
    if stream.is_null() {
        return -1;
    }
    let f = &mut *stream;
    f.mode = match mode {
        _IOFBF => BufferMode::Full,
        _IOLBF => BufferMode::Line,
        _IONBF => BufferMode::None,
        _ => return -1,
    };
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fileno(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return -1;
    }
    (*stream).fd
}
