use crate::pal::Pal;

use super::{
    BufferMode, EOF, FILE, FILE_FLAG_EOF, FILE_FLAG_ERR, FILE_FLAG_READABLE, FILE_FLAG_WRITABLE,
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fgetc(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }

    let f = &mut *stream;
    if f.flags & FILE_FLAG_READABLE == 0 {
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fputc(c: i32, stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return EOF;
    }

    let f = &mut *stream;
    if f.flags & FILE_FLAG_WRITABLE == 0 {
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fgets(s: *mut u8, n: i32, stream: *mut FILE) -> *mut u8 {
    if s.is_null() || stream.is_null() || n <= 0 {
        return core::ptr::null_mut();
    }

    let max = (n - 1) as usize;
    let mut i = 0usize;

    while i < max {
        let c = fgetc(stream);
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fputs(s: *const u8, stream: *mut FILE) -> i32 {
    if s.is_null() || stream.is_null() {
        return EOF;
    }

    let mut p = s;
    while *p != 0 {
        if fputc(*p as i32, stream) == EOF {
            return EOF;
        }
        p = p.add(1);
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ungetc(c: i32, stream: *mut FILE) -> i32 {
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getchar() -> i32 {
    fgetc(super::streams::stdin_file())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn putchar(c: i32) -> i32 {
    fputc(c, super::streams::stdout_file())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn puts(s: *const u8) -> i32 {
    if s.is_null() {
        return EOF;
    }
    let out = super::streams::stdout_file();
    if fputs(s, out) == EOF {
        return EOF;
    }
    fputc(b'\n' as i32, out)
}
