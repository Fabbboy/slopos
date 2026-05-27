use core::ffi::VaList;

use super::chars::fgetc;
use super::streams;
use super::{EOF, FILE};

#[inline(always)]
fn is_whitespace(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\n' || c == b'\r'
}

unsafe fn vsscanf_impl(input: *const u8, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    let mut matched: i32 = 0;
    let mut ip = input;
    let mut fp = fmt;

    while *fp != 0 {
        if is_whitespace(*fp) {
            fp = fp.add(1);
            while *ip != 0 && is_whitespace(*ip) {
                ip = ip.add(1);
            }
            continue;
        }

        if *fp != b'%' {
            if *ip == 0 || *ip != *fp {
                break;
            }
            ip = ip.add(1);
            fp = fp.add(1);
            continue;
        }

        fp = fp.add(1);
        if *fp == 0 {
            break;
        }

        let mut length_long = false;
        if *fp == b'l' {
            length_long = true;
            fp = fp.add(1);
        }

        let spec = *fp;
        if spec == 0 {
            break;
        }
        fp = fp.add(1);

        match spec {
            b'd' | b'i' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let mut neg = false;
                if *ip == b'-' {
                    neg = true;
                    ip = ip.add(1);
                } else if *ip == b'+' {
                    ip = ip.add(1);
                }

                if !(*ip).is_ascii_digit() {
                    return matched;
                }

                let mut val: i64 = 0;
                while (*ip).is_ascii_digit() {
                    val = val.wrapping_mul(10).wrapping_add((*ip - b'0') as i64);
                    ip = ip.add(1);
                }
                if neg {
                    val = -val;
                }

                if length_long {
                    let ptr = ap.next_arg::<*mut i64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut i32>();
                    if !ptr.is_null() {
                        *ptr = val as i32;
                    }
                }
                matched += 1;
            }

            b'u' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                if !(*ip).is_ascii_digit() {
                    return matched;
                }

                let mut val: u64 = 0;
                while (*ip).is_ascii_digit() {
                    val = val.wrapping_mul(10).wrapping_add((*ip - b'0') as u64);
                    ip = ip.add(1);
                }

                if length_long {
                    let ptr = ap.next_arg::<*mut u64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut u32>();
                    if !ptr.is_null() {
                        *ptr = val as u32;
                    }
                }
                matched += 1;
            }

            b'x' | b'X' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                if *ip == b'0' && (*ip.add(1) == b'x' || *ip.add(1) == b'X') {
                    ip = ip.add(2);
                }

                let start = ip;
                let mut val: u64 = 0;
                loop {
                    let c = *ip;
                    let d = if c.is_ascii_digit() {
                        (c - b'0') as u64
                    } else if (b'a'..=b'f').contains(&c) {
                        (c - b'a' + 10) as u64
                    } else if (b'A'..=b'F').contains(&c) {
                        (c - b'A' + 10) as u64
                    } else {
                        break;
                    };
                    val = val.wrapping_mul(16).wrapping_add(d);
                    ip = ip.add(1);
                }

                if ip == start {
                    return matched;
                }

                if length_long {
                    let ptr = ap.next_arg::<*mut u64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut u32>();
                    if !ptr.is_null() {
                        *ptr = val as u32;
                    }
                }
                matched += 1;
            }

            b's' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let dst = ap.next_arg::<*mut u8>();
                if dst.is_null() {
                    return matched;
                }

                let mut i = 0usize;
                while *ip != 0 && !is_whitespace(*ip) {
                    *dst.add(i) = *ip;
                    ip = ip.add(1);
                    i += 1;
                }
                *dst.add(i) = 0;
                matched += 1;
            }

            b'c' => {
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let ptr = ap.next_arg::<*mut u8>();
                if !ptr.is_null() {
                    *ptr = *ip;
                }
                ip = ip.add(1);
                matched += 1;
            }

            b'%' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip != b'%' {
                    break;
                }
                ip = ip.add(1);
            }

            _ => break,
        }
    }

    if matched == 0 && *ip == 0 {
        return EOF;
    }
    matched
}

unsafe fn vfscanf_impl(stream: *mut FILE, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    let mut matched: i32 = 0;
    let mut fp = fmt;

    while *fp != 0 {
        if is_whitespace(*fp) {
            fp = fp.add(1);
            loop {
                let c = fgetc(stream);
                if c == EOF {
                    break;
                }
                if !is_whitespace(c as u8) {
                    super::chars::ungetc(c, stream);
                    break;
                }
            }
            continue;
        }

        if *fp != b'%' {
            let c = fgetc(stream);
            if c == EOF || c as u8 != *fp {
                break;
            }
            fp = fp.add(1);
            continue;
        }

        fp = fp.add(1);
        if *fp == 0 {
            break;
        }

        let mut length_long = false;
        if *fp == b'l' {
            length_long = true;
            fp = fp.add(1);
        }

        let spec = *fp;
        if spec == 0 {
            break;
        }
        fp = fp.add(1);

        match spec {
            b'd' | b'i' => {
                loop {
                    let c = fgetc(stream);
                    if c == EOF {
                        break;
                    }
                    if !is_whitespace(c as u8) {
                        super::chars::ungetc(c, stream);
                        break;
                    }
                }

                let mut neg = false;
                let c = fgetc(stream);
                if c == EOF {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }
                if c as u8 == b'-' {
                    neg = true;
                } else if c as u8 == b'+' {
                } else if (c as u8).is_ascii_digit() {
                    super::chars::ungetc(c, stream);
                } else {
                    super::chars::ungetc(c, stream);
                    return matched;
                }

                let first = fgetc(stream);
                if first == EOF || !(first as u8).is_ascii_digit() {
                    if first != EOF {
                        super::chars::ungetc(first, stream);
                    }
                    return matched;
                }

                let mut val: i64 = (first as u8 - b'0') as i64;
                loop {
                    let d = fgetc(stream);
                    if d == EOF || !(d as u8).is_ascii_digit() {
                        if d != EOF {
                            super::chars::ungetc(d, stream);
                        }
                        break;
                    }
                    val = val.wrapping_mul(10).wrapping_add((d as u8 - b'0') as i64);
                }
                if neg {
                    val = -val;
                }

                if length_long {
                    let ptr = ap.next_arg::<*mut i64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut i32>();
                    if !ptr.is_null() {
                        *ptr = val as i32;
                    }
                }
                matched += 1;
            }

            b'u' => {
                loop {
                    let c = fgetc(stream);
                    if c == EOF {
                        break;
                    }
                    if !is_whitespace(c as u8) {
                        super::chars::ungetc(c, stream);
                        break;
                    }
                }

                let first = fgetc(stream);
                if first == EOF || !(first as u8).is_ascii_digit() {
                    if first != EOF {
                        super::chars::ungetc(first, stream);
                    }
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let mut val: u64 = (first as u8 - b'0') as u64;
                loop {
                    let d = fgetc(stream);
                    if d == EOF || !(d as u8).is_ascii_digit() {
                        if d != EOF {
                            super::chars::ungetc(d, stream);
                        }
                        break;
                    }
                    val = val.wrapping_mul(10).wrapping_add((d as u8 - b'0') as u64);
                }

                if length_long {
                    let ptr = ap.next_arg::<*mut u64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut u32>();
                    if !ptr.is_null() {
                        *ptr = val as u32;
                    }
                }
                matched += 1;
            }

            b's' => {
                loop {
                    let c = fgetc(stream);
                    if c == EOF {
                        break;
                    }
                    if !is_whitespace(c as u8) {
                        super::chars::ungetc(c, stream);
                        break;
                    }
                }

                let dst = ap.next_arg::<*mut u8>();
                if dst.is_null() {
                    return matched;
                }

                let first = fgetc(stream);
                if first == EOF {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let mut i = 0usize;
                *dst.add(i) = first as u8;
                i += 1;

                loop {
                    let c = fgetc(stream);
                    if c == EOF || is_whitespace(c as u8) {
                        if c != EOF {
                            super::chars::ungetc(c, stream);
                        }
                        break;
                    }
                    *dst.add(i) = c as u8;
                    i += 1;
                }
                *dst.add(i) = 0;
                matched += 1;
            }

            b'c' => {
                let c = fgetc(stream);
                if c == EOF {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }
                let ptr = ap.next_arg::<*mut u8>();
                if !ptr.is_null() {
                    *ptr = c as u8;
                }
                matched += 1;
            }

            _ => break,
        }
    }

    if matched == 0 {
        return EOF;
    }
    matched
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sscanf(buf: *const u8, fmt: *const u8, mut args: ...) -> i32 {
    vsscanf_impl(buf, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fscanf(stream: *mut FILE, fmt: *const u8, mut args: ...) -> i32 {
    vfscanf_impl(stream, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scanf(fmt: *const u8, mut args: ...) -> i32 {
    vfscanf_impl(streams::stdin_file(), fmt, &mut args)
}
