use core::ffi::VaList;

use super::FILE;
use super::chars::fputc_unlocked;
use super::streams;

const FLAG_LEFT: u32 = 1;
const FLAG_ZERO: u32 = 2;
const FLAG_PLUS: u32 = 4;
const FLAG_SPACE: u32 = 8;
const FLAG_ALT: u32 = 16;

const DIGITS_LOWER: &[u8; 16] = b"0123456789abcdef";
const DIGITS_UPPER: &[u8; 16] = b"0123456789ABCDEF";

#[derive(Clone, Copy, PartialEq)]
enum Length {
    Default,
    Long,
    LongLong,
    SizeT,
    PtrdiffT,
}

unsafe fn write_unsigned(value: u64, base: u64, digits: &[u8; 16], buf: &mut [u8; 22]) -> usize {
    if value == 0 {
        buf[21] = b'0';
        return 1;
    }
    let mut pos = 22usize;
    let mut v = value;
    while v > 0 {
        pos -= 1;
        buf[pos] = digits[(v % base) as usize];
        v /= base;
    }
    22 - pos
}

unsafe fn format_to_cb<F: FnMut(u8)>(out: &mut F, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    let mut count: i32 = 0;
    let mut p = fmt;

    macro_rules! emit {
        ($byte:expr) => {{
            out($byte);
            count += 1;
        }};
    }

    macro_rules! emit_pad {
        ($byte:expr, $n:expr) => {{
            let mut _i = 0i32;
            while _i < $n {
                emit!($byte);
                _i += 1;
            }
        }};
    }

    while *p != 0 {
        if *p != b'%' {
            emit!(*p);
            p = p.add(1);
            continue;
        }
        p = p.add(1); // skip '%'

        if *p == 0 {
            break;
        }
        if *p == b'%' {
            emit!(b'%');
            p = p.add(1);
            continue;
        }

        // --- Parse flags ---
        let mut flags: u32 = 0;
        loop {
            match *p {
                b'-' => flags |= FLAG_LEFT,
                b'0' => flags |= FLAG_ZERO,
                b'+' => flags |= FLAG_PLUS,
                b' ' => flags |= FLAG_SPACE,
                b'#' => flags |= FLAG_ALT,
                _ => break,
            }
            p = p.add(1);
        }

        // --- Parse width ---
        let mut width: i32 = 0;
        while (*p).is_ascii_digit() {
            width = width * 10 + (*p - b'0') as i32;
            p = p.add(1);
        }

        // --- Parse precision ---
        let mut precision: i32 = -1;
        if *p == b'.' {
            p = p.add(1);
            precision = 0;
            while (*p).is_ascii_digit() {
                precision = precision * 10 + (*p - b'0') as i32;
                p = p.add(1);
            }
        }

        // --- Parse length modifier ---
        let mut length = Length::Default;
        match *p {
            b'l' => {
                p = p.add(1);
                if *p == b'l' {
                    length = Length::LongLong;
                    p = p.add(1);
                } else {
                    length = Length::Long;
                }
            }
            b'z' => {
                length = Length::SizeT;
                p = p.add(1);
            }
            b't' => {
                length = Length::PtrdiffT;
                p = p.add(1);
            }
            b'h' => {
                p = p.add(1);
                if *p == b'h' {
                    p = p.add(1);
                }
                // treat h and hh as default (promoted to int in varargs)
            }
            _ => {}
        }

        // --- Handle specifier ---
        let spec = *p;
        if spec == 0 {
            break;
        }
        p = p.add(1);

        match spec {
            b'd' | b'i' => {
                let val: i64 = match length {
                    Length::LongLong | Length::SizeT | Length::PtrdiffT => ap.next_arg::<i64>(),
                    Length::Long => ap.next_arg::<i64>(),
                    Length::Default => ap.next_arg::<i32>() as i64,
                };

                let negative = val < 0;
                let abs_val = val.unsigned_abs();

                let mut num_buf = [0u8; 22];
                let num_len = write_unsigned(abs_val, 10, DIGITS_LOWER, &mut num_buf);
                let num_start = 22 - num_len;

                let sign: Option<u8> = if negative {
                    Some(b'-')
                } else if flags & FLAG_PLUS != 0 {
                    Some(b'+')
                } else if flags & FLAG_SPACE != 0 {
                    Some(b' ')
                } else {
                    None
                };

                let min_digits = if precision >= 0 {
                    precision as usize
                } else {
                    1
                };
                let zero_fill = if num_len < min_digits {
                    min_digits - num_len
                } else {
                    0
                };

                let sign_len = if sign.is_some() { 1 } else { 0 };
                let content_len = sign_len + zero_fill + num_len;
                let pad = if (width as usize) > content_len {
                    width as usize - content_len
                } else {
                    0
                };

                let use_zero_pad =
                    flags & FLAG_ZERO != 0 && flags & FLAG_LEFT == 0 && precision < 0;

                if flags & FLAG_LEFT == 0 && !use_zero_pad {
                    emit_pad!(b' ', pad as i32);
                }
                if let Some(s) = sign {
                    emit!(s);
                }
                if use_zero_pad {
                    emit_pad!(b'0', pad as i32);
                }
                emit_pad!(b'0', zero_fill as i32);
                for i in 0..num_len {
                    emit!(num_buf[num_start + i]);
                }
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad as i32);
                }
            }

            b'u' | b'o' | b'x' | b'X' => {
                let val: u64 = match length {
                    Length::LongLong | Length::SizeT | Length::PtrdiffT => ap.next_arg::<u64>(),
                    Length::Long => ap.next_arg::<u64>(),
                    Length::Default => ap.next_arg::<u32>() as u64,
                };

                let (base, digits): (u64, &[u8; 16]) = match spec {
                    b'o' => (8, DIGITS_LOWER),
                    b'x' => (16, DIGITS_LOWER),
                    b'X' => (16, DIGITS_UPPER),
                    _ => (10, DIGITS_LOWER),
                };

                let mut num_buf = [0u8; 22];
                let num_len = write_unsigned(val, base, digits, &mut num_buf);
                let num_start = 22 - num_len;

                let prefix: &[u8] = if flags & FLAG_ALT != 0 && val != 0 {
                    match spec {
                        b'o' => b"0",
                        b'x' => b"0x",
                        b'X' => b"0X",
                        _ => b"",
                    }
                } else {
                    b""
                };

                let min_digits = if precision >= 0 {
                    precision as usize
                } else {
                    1
                };
                let zero_fill = if num_len < min_digits {
                    min_digits - num_len
                } else {
                    0
                };

                let content_len = prefix.len() + zero_fill + num_len;
                let pad = if (width as usize) > content_len {
                    width as usize - content_len
                } else {
                    0
                };

                let use_zero_pad =
                    flags & FLAG_ZERO != 0 && flags & FLAG_LEFT == 0 && precision < 0;

                if flags & FLAG_LEFT == 0 && !use_zero_pad {
                    emit_pad!(b' ', pad as i32);
                }
                for &b in prefix {
                    emit!(b);
                }
                if use_zero_pad {
                    emit_pad!(b'0', pad as i32);
                }
                emit_pad!(b'0', zero_fill as i32);
                for i in 0..num_len {
                    emit!(num_buf[num_start + i]);
                }
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad as i32);
                }
            }

            b's' => {
                let s_ptr: *const u8 = ap.next_arg::<*const u8>();
                let null_str = b"(null)\0";
                let actual = if s_ptr.is_null() {
                    null_str.as_ptr()
                } else {
                    s_ptr
                };

                let mut slen = 0usize;
                let mut q = actual;
                while *q != 0 {
                    slen += 1;
                    q = q.add(1);
                }

                if precision >= 0 && (precision as usize) < slen {
                    slen = precision as usize;
                }

                let pad = if (width as usize) > slen {
                    width as usize - slen
                } else {
                    0
                };

                if flags & FLAG_LEFT == 0 {
                    emit_pad!(b' ', pad as i32);
                }
                for i in 0..slen {
                    emit!(*actual.add(i));
                }
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad as i32);
                }
            }

            b'c' => {
                let c = ap.next_arg::<i32>() as u8;
                let pad = if width > 1 { width - 1 } else { 0 };

                if flags & FLAG_LEFT == 0 {
                    emit_pad!(b' ', pad);
                }
                emit!(c);
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad);
                }
            }

            b'p' => {
                let ptr_val = ap.next_arg::<usize>() as u64;
                let mut num_buf = [0u8; 22];
                let num_len = write_unsigned(ptr_val, 16, DIGITS_LOWER, &mut num_buf);
                let num_start = 22 - num_len;

                emit!(b'0');
                emit!(b'x');
                for i in 0..num_len {
                    emit!(num_buf[num_start + i]);
                }
            }

            _ => {
                emit!(b'%');
                emit!(spec);
            }
        }
    }

    count
}

// ---------------------------------------------------------------------------
// Public printf family
// ---------------------------------------------------------------------------

unsafe fn vfprintf_impl(stream: *mut FILE, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    if stream.is_null() {
        return -1;
    }
    // One acquisition for the whole conversion: POSIX §2.5.1 requires the call
    // to be atomic against other stdio on the stream, and taking the lock here
    // rather than inside the emit callback keeps it off the per-byte path.
    (*stream).lock.lock();
    let count = format_to_cb(
        &mut |b: u8| {
            fputc_unlocked(b as i32, stream);
        },
        fmt,
        ap,
    );
    (*stream).lock.unlock();
    count
}

unsafe fn vsnprintf_impl(buf: *mut u8, n: usize, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    let mut pos: usize = 0;
    let limit = if n > 0 { n - 1 } else { 0 };

    let total = format_to_cb(
        &mut |b: u8| {
            if pos < limit {
                *buf.add(pos) = b;
            }
            pos += 1;
        },
        fmt,
        ap,
    );

    if n > 0 {
        let term = if pos < limit { pos } else { limit };
        *buf.add(term) = 0;
    }

    total
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn printf(fmt: *const u8, mut args: ...) -> i32 {
    vfprintf_impl(streams::stdout_file(), fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fprintf(stream: *mut FILE, fmt: *const u8, mut args: ...) -> i32 {
    vfprintf_impl(stream, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sprintf(buf: *mut u8, fmt: *const u8, mut args: ...) -> i32 {
    vsnprintf_impl(buf, usize::MAX, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn snprintf(buf: *mut u8, n: usize, fmt: *const u8, mut args: ...) -> i32 {
    vsnprintf_impl(buf, n, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vprintf(fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vfprintf_impl(streams::stdout_file(), fmt, &mut ap)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vfprintf(stream: *mut FILE, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vfprintf_impl(stream, fmt, &mut ap)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsprintf(buf: *mut u8, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vsnprintf_impl(buf, usize::MAX, fmt, &mut ap)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsnprintf(
    buf: *mut u8,
    n: usize,
    fmt: *const u8,
    mut ap: VaList<'_>,
) -> i32 {
    vsnprintf_impl(buf, n, fmt, &mut ap)
}
