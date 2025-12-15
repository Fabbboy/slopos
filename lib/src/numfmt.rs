use core::ffi::{c_char, c_int};

use crate::memory;
use crate::string;

const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

#[inline(always)]
fn to_u8(c: c_char) -> u8 {
    c as u8
}

unsafe fn ensure_null(buffer: *mut c_char, buffer_len: usize) {
    unsafe {
        if !buffer.is_null() && buffer_len > 0 {
            *buffer = 0;
        }
    }
}

pub unsafe fn u64_to_decimal_internal(value: u64, buffer: *mut c_char, buffer_len: usize) -> usize {
    if buffer.is_null() || buffer_len == 0 {
        return 0;
    }

    unsafe {
        let mut write_pos = buffer_len - 1;
        *buffer.add(write_pos) = 0;

        if value == 0 {
            if write_pos == 0 {
                *buffer = 0;
                return 0;
            }
            write_pos -= 1;
            *buffer.add(write_pos) = b'0' as c_char;
        } else {
            let mut v = value;
            while v > 0 {
                if write_pos == 0 {
                    *buffer = 0;
                    return 0;
                }
                write_pos -= 1;
                *buffer.add(write_pos) = (b'0' + (v % 10) as u8) as c_char;
                v /= 10;
            }
        }

        let len = (buffer_len - 1) - write_pos;
        memory::memmove_internal(
            buffer as *mut u8,
            buffer.add(write_pos) as *const u8,
            len + 1,
        );
        len
    }
}

pub unsafe fn i64_to_decimal_internal(value: i64, buffer: *mut c_char, buffer_len: usize) -> usize {
    if buffer.is_null() || buffer_len == 0 {
        return 0;
    }

    unsafe {
        if value >= 0 {
            return u64_to_decimal_internal(value as u64, buffer, buffer_len);
        }

        if buffer_len < 2 {
            *buffer = 0;
            return 0;
        }

        *buffer = b'-' as c_char;
        let magnitude = if value == i64::MIN {
            (i64::MAX as u64) + 1
        } else {
            (-value) as u64
        };

        let len = u64_to_decimal_internal(magnitude, buffer.add(1), buffer_len - 1);
        if len == 0 {
            *buffer = 0;
            return 0;
        }
        len + 1
    }
}

pub unsafe fn u64_to_hex_internal(
    value: u64,
    buffer: *mut c_char,
    buffer_len: usize,
    with_prefix: bool,
) -> usize {
    if buffer.is_null() || buffer_len == 0 {
        return 0;
    }

    unsafe {
        let needed = 16 + if with_prefix { 2 } else { 0 } + 1;
        if buffer_len < needed {
            *buffer = 0;
            return 0;
        }

        let mut pos = 0usize;
        if with_prefix {
            *buffer.add(pos) = b'0' as c_char;
            pos += 1;
            *buffer.add(pos) = b'x' as c_char;
            pos += 1;
        }

        let mut i = 16;
        while i > 0 {
            i -= 1;
            let digit = ((value >> (i * 4)) & 0xF) as usize;
            *buffer.add(pos) = HEX_DIGITS[digit] as c_char;
            pos += 1;
        }

        *buffer.add(pos) = 0;
        pos
    }
}

pub unsafe fn u8_to_hex_internal(value: u8, buffer: *mut c_char, buffer_len: usize) -> usize {
    if buffer.is_null() || buffer_len < 3 {
        unsafe { ensure_null(buffer, buffer_len) };
        return 0;
    }

    unsafe {
        *buffer.add(0) = HEX_DIGITS[((value >> 4) & 0xF) as usize] as c_char;
        *buffer.add(1) = HEX_DIGITS[(value & 0xF) as usize] as c_char;
        *buffer.add(2) = 0;
        2
    }
}

pub unsafe fn parse_u32_internal(str_ptr: *const c_char, out: *mut u32, fallback: u32) -> c_int {
    unsafe {
        if out.is_null() {
            return -1;
        }
        if str_ptr.is_null() {
            *out = fallback;
            return -1;
        }

        let mut cursor = str_ptr;
        while *cursor != 0 && string::isspace(to_u8(*cursor)) {
            cursor = cursor.add(1);
        }

        let mut value: u64 = 0;
        let mut seen_digit = false;

        while *cursor != 0 && string::isdigit(to_u8(*cursor)) {
            seen_digit = true;
            value = value
                .saturating_mul(10)
                .saturating_add((to_u8(*cursor) - b'0') as u64);
            if value > u32::MAX as u64 {
                value = u32::MAX as u64;
            }
            cursor = cursor.add(1);
        }

        if *cursor != 0 {
            if (*cursor == b'm' as c_char || *cursor == b'M' as c_char)
                && (*cursor.add(1) == b's' as c_char || *cursor.add(1) == b'S' as c_char)
            {
                cursor = cursor.add(2);
            }
        }

        while *cursor != 0 && string::isspace(to_u8(*cursor)) {
            cursor = cursor.add(1);
        }

        if !seen_digit || *cursor != 0 {
            *out = fallback;
            return -1;
        }

        *out = value as u32;
        0
    }
}

pub unsafe fn parse_u64_internal(str_ptr: *const c_char, out: *mut u64, fallback: u64) -> c_int {
    unsafe {
        if out.is_null() {
            return -1;
        }
        if str_ptr.is_null() {
            *out = fallback;
            return -1;
        }

        let mut cursor = str_ptr;
        while *cursor != 0 && string::isspace(to_u8(*cursor)) {
            cursor = cursor.add(1);
        }

        let mut value: u64 = 0;
        let mut seen_digit = false;

        while *cursor != 0 && string::isdigit(to_u8(*cursor)) {
            seen_digit = true;
            value = value
                .saturating_mul(10)
                .saturating_add((to_u8(*cursor) - b'0') as u64);
            cursor = cursor.add(1);
        }

        if *cursor != 0 {
            if (*cursor == b'm' as c_char || *cursor == b'M' as c_char)
                && (*cursor.add(1) == b's' as c_char || *cursor.add(1) == b'S' as c_char)
            {
                cursor = cursor.add(2);
            }
        }

        while *cursor != 0 && string::isspace(to_u8(*cursor)) {
            cursor = cursor.add(1);
        }

        if !seen_digit || *cursor != 0 {
            *out = fallback;
            return -1;
        }

        *out = value;
        0
    }
}
pub fn numfmt_u64_to_decimal(value: u64, buffer: *mut c_char, buffer_len: usize) -> usize {
    unsafe { u64_to_decimal_internal(value, buffer, buffer_len) }
}
pub fn numfmt_i64_to_decimal(value: i64, buffer: *mut c_char, buffer_len: usize) -> usize {
    unsafe { i64_to_decimal_internal(value, buffer, buffer_len) }
}
pub fn numfmt_u64_to_hex(
    value: u64,
    buffer: *mut c_char,
    buffer_len: usize,
    with_prefix: c_int,
) -> usize {
    unsafe { u64_to_hex_internal(value, buffer, buffer_len, with_prefix != 0) }
}
pub fn numfmt_u8_to_hex(value: u8, buffer: *mut c_char, buffer_len: usize) -> usize {
    unsafe { u8_to_hex_internal(value, buffer, buffer_len) }
}
pub fn numfmt_parse_u32(str_ptr: *const c_char, out: *mut u32, fallback: u32) -> c_int {
    unsafe { parse_u32_internal(str_ptr, out, fallback) }
}
pub fn numfmt_parse_u64(str_ptr: *const c_char, out: *mut u64, fallback: u64) -> c_int {
    unsafe { parse_u64_internal(str_ptr, out, fallback) }
}
