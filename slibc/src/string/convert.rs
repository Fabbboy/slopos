#[inline(always)]
fn is_space(b: u8) -> bool {
    b <= 0x20
}

#[inline(always)]
fn digit_value(b: u8) -> i32 {
    if b.is_ascii_digit() {
        (b - b'0') as i32
    } else if b.is_ascii_lowercase() {
        (b - b'a' + 10) as i32
    } else if b.is_ascii_uppercase() {
        (b - b'A' + 10) as i32
    } else {
        -1
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn atoi(s: *const u8) -> i32 {
    strtol(s, core::ptr::null_mut(), 10) as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn atol(s: *const u8) -> i64 {
    strtol(s, core::ptr::null_mut(), 10)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtol(s: *const u8, endptr: *mut *const u8, base: i32) -> i64 {
    if s.is_null() {
        if !endptr.is_null() {
            *endptr = core::ptr::null();
        }
        return 0;
    }

    let original = s;
    let mut p = s;

    while *p != 0 && is_space(*p) {
        p = p.add(1);
    }

    let mut neg = false;
    if *p == b'+' {
        p = p.add(1);
    } else if *p == b'-' {
        neg = true;
        p = p.add(1);
    }

    let mut b = base;
    if b == 0 {
        if *p == b'0' {
            if *p.add(1) == b'x' || *p.add(1) == b'X' {
                b = 16;
                p = p.add(2);
            } else {
                b = 8;
            }
        } else {
            b = 10;
        }
    } else if b == 16 {
        if *p == b'0' && (*p.add(1) == b'x' || *p.add(1) == b'X') {
            p = p.add(2);
        }
    }

    if !(2..=36).contains(&b) {
        if !endptr.is_null() {
            *endptr = original;
        }
        return 0;
    }

    let digits_start = p;
    let mut value: u64 = 0;

    loop {
        let ch = *p;
        if ch == 0 {
            break;
        }
        let d = digit_value(ch);
        if d < 0 || d >= b {
            break;
        }
        value = value.wrapping_mul(b as u64).wrapping_add(d as u64);
        p = p.add(1);
    }

    if p == digits_start {
        if !endptr.is_null() {
            *endptr = original;
        }
        return 0;
    }

    if !endptr.is_null() {
        *endptr = p;
    }

    if neg { -(value as i64) } else { value as i64 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtoul(s: *const u8, endptr: *mut *const u8, base: i32) -> u64 {
    if s.is_null() {
        if !endptr.is_null() {
            *endptr = core::ptr::null();
        }
        return 0;
    }

    let original = s;
    let mut p = s;

    while *p != 0 && is_space(*p) {
        p = p.add(1);
    }

    let mut neg = false;
    if *p == b'+' {
        p = p.add(1);
    } else if *p == b'-' {
        neg = true;
        p = p.add(1);
    }

    let mut b = base;
    if b == 0 {
        if *p == b'0' {
            if *p.add(1) == b'x' || *p.add(1) == b'X' {
                b = 16;
                p = p.add(2);
            } else {
                b = 8;
            }
        } else {
            b = 10;
        }
    } else if b == 16 {
        if *p == b'0' && (*p.add(1) == b'x' || *p.add(1) == b'X') {
            p = p.add(2);
        }
    }

    if !(2..=36).contains(&b) {
        if !endptr.is_null() {
            *endptr = original;
        }
        return 0;
    }

    let digits_start = p;
    let mut value: u64 = 0;

    loop {
        let ch = *p;
        if ch == 0 {
            break;
        }
        let d = digit_value(ch);
        if d < 0 || d >= b {
            break;
        }
        value = value.wrapping_mul(b as u64).wrapping_add(d as u64);
        p = p.add(1);
    }

    if p == digits_start {
        if !endptr.is_null() {
            *endptr = original;
        }
        return 0;
    }

    if !endptr.is_null() {
        *endptr = p;
    }

    if neg { 0u64.wrapping_sub(value) } else { value }
}

pub fn itoa_buf(n: i64, buf: *mut u8, base: u32) -> *mut u8 {
    if buf.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        if !(2..=36).contains(&base) {
            *buf = 0;
            return buf;
        }

        let mut p = buf.add(64);
        *p = 0;

        let negative = base == 10 && n < 0;
        let mut value = if negative { n.unsigned_abs() } else { n as u64 };

        loop {
            let d = (value % base as u64) as u8;
            p = p.sub(1);
            *p = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
            value /= base as u64;
            if value == 0 {
                break;
            }
        }

        if negative {
            p = p.sub(1);
            *p = b'-';
        }

        p
    }
}
