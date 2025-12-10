use core::ffi::{c_char, c_int};
use core::ptr;

#[inline(always)]
fn to_u8(c: c_char) -> u8 {
    c as u8
}

#[inline(always)]
fn from_bool(val: bool) -> c_int {
    if val { 1 } else { 0 }
}

pub fn isspace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\x0b')
}

pub fn isdigit(byte: u8) -> bool {
    (b'0'..=b'9').contains(&byte)
}

pub fn tolower(byte: u8) -> u8 {
    if (b'A'..=b'Z').contains(&byte) {
        byte - b'A' + b'a'
    } else {
        byte
    }
}

pub fn toupper(byte: u8) -> u8 {
    if (b'a'..=b'z').contains(&byte) {
        byte - b'a' + b'A'
    } else {
        byte
    }
}

pub unsafe fn strlen_internal(ptr: *const c_char) -> usize {
    unsafe {
        if ptr.is_null() {
            return 0;
        }

        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        len
    }
}

pub unsafe fn strcmp_internal(lhs: *const c_char, rhs: *const c_char) -> c_int {
    unsafe {
        if lhs == rhs {
            return 0;
        }
        if lhs.is_null() {
            return -1;
        }
        if rhs.is_null() {
            return 1;
        }

        let mut l = lhs;
        let mut r = rhs;
        while *l != 0 && *l == *r {
            l = l.add(1);
            r = r.add(1);
        }

        to_u8(*l) as c_int - to_u8(*r) as c_int
    }
}

pub unsafe fn strncmp_internal(lhs: *const c_char, rhs: *const c_char, mut n: usize) -> c_int {
    unsafe {
        if n == 0 {
            return 0;
        }

        if lhs.is_null() {
            return if rhs.is_null() { 0 } else { -1 };
        }
        if rhs.is_null() {
            return 1;
        }

        let mut l = lhs;
        let mut r = rhs;

        while n > 0 && *l == *r {
            if *l == 0 {
                return 0;
            }
            l = l.add(1);
            r = r.add(1);
            n -= 1;
        }

        if n == 0 {
            0
        } else {
            to_u8(*l) as c_int - to_u8(*r) as c_int
        }
    }
}

pub unsafe fn strcpy_internal(dest: *mut c_char, src: *const c_char) -> *mut c_char {
    unsafe {
        if dest.is_null() || src.is_null() {
            return dest;
        }

        let mut d = dest;
        let mut s = src;
        loop {
            let ch = *s;
            *d = ch;
            if ch == 0 {
                break;
            }
            d = d.add(1);
            s = s.add(1);
        }
        dest
    }
}

pub unsafe fn strncpy_internal(dest: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
    unsafe {
        if dest.is_null() || n == 0 {
            return dest;
        }

        let mut i = 0usize;
        while i < n {
            let ch = if !src.is_null() { *src.add(i) } else { 0 };
            *dest.add(i) = ch;
            i += 1;
            if ch == 0 {
                break;
            }
        }

        while i < n {
            *dest.add(i) = 0;
            i += 1;
        }

        dest
    }
}

pub unsafe fn strcasecmp_internal(lhs: *const c_char, rhs: *const c_char) -> c_int {
    unsafe {
        if lhs == rhs {
            return 0;
        }
        if lhs.is_null() {
            return -1;
        }
        if rhs.is_null() {
            return 1;
        }

        let mut l = lhs;
        let mut r = rhs;
        while *l != 0 && *r != 0 {
            let ll = tolower(to_u8(*l));
            let rr = tolower(to_u8(*r));
            if ll != rr {
                return ll as c_int - rr as c_int;
            }
            l = l.add(1);
            r = r.add(1);
        }

        to_u8(*l) as c_int - to_u8(*r) as c_int
    }
}

pub unsafe fn strncasecmp_internal(lhs: *const c_char, rhs: *const c_char, n: usize) -> c_int {
    unsafe {
        if n == 0 {
            return 0;
        }
        if lhs.is_null() {
            return if rhs.is_null() { 0 } else { -1 };
        }
        if rhs.is_null() {
            return 1;
        }

        let mut idx = 0usize;
        while idx < n && *lhs.add(idx) != 0 && *rhs.add(idx) != 0 {
            let ll = tolower(to_u8(*lhs.add(idx)));
            let rr = tolower(to_u8(*rhs.add(idx)));
            if ll != rr {
                return ll as c_int - rr as c_int;
            }
            idx += 1;
        }

        if idx == n {
            return 0;
        }

        to_u8(*lhs.add(idx)) as c_int - to_u8(*rhs.add(idx)) as c_int
    }
}

pub unsafe fn strchr_internal(str: *const c_char, c: c_int) -> *mut c_char {
    unsafe {
        if str.is_null() {
            return ptr::null_mut();
        }
        let target = c as u8;
        let mut cursor = str;
        while *cursor != 0 {
            if to_u8(*cursor) == target {
                return cursor as *mut c_char;
            }
            cursor = cursor.add(1);
        }
        if target == 0 {
            cursor as *mut c_char
        } else {
            ptr::null_mut()
        }
    }
}

pub unsafe fn strstr_internal(haystack: *const c_char, needle: *const c_char) -> *mut c_char {
    unsafe {
        if haystack.is_null() || needle.is_null() {
            return ptr::null_mut();
        }

        if *needle == 0 {
            return haystack as *mut c_char;
        }

        let needle_len = strlen_internal(needle);
        let mut h = haystack;
        while *h != 0 {
            if *h == *needle {
                if strncmp_internal(h, needle, needle_len) == 0 {
                    return h as *mut c_char;
                }
            }
            h = h.add(1);
        }
        ptr::null_mut()
    }
}

pub unsafe fn str_has_token_internal(str: *const c_char, token: *const c_char) -> c_int {
    unsafe {
        if str.is_null() || token.is_null() {
            return 0;
        }

        let token_len = strlen_internal(token);
        if token_len == 0 {
            return 0;
        }

        let mut cursor = str;
        while *cursor != 0 {
            while *cursor != 0 && isspace(to_u8(*cursor)) {
                cursor = cursor.add(1);
            }
            if *cursor == 0 {
                break;
            }

            let start = cursor;
            while *cursor != 0 && !isspace(to_u8(*cursor)) {
                cursor = cursor.add(1);
            }

            let len = cursor as usize - start as usize;
            if len == token_len && strncmp_internal(start, token, token_len) == 0 {
                return 1;
            }
        }
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn strlen(str: *const c_char) -> usize {
    unsafe { strlen_internal(str) }
}

#[no_mangle]
pub unsafe extern "C" fn strcmp(lhs: *const c_char, rhs: *const c_char) -> c_int {
    unsafe { strcmp_internal(lhs, rhs) }
}

#[no_mangle]
pub unsafe extern "C" fn strncmp(lhs: *const c_char, rhs: *const c_char, n: usize) -> c_int {
    unsafe { strncmp_internal(lhs, rhs, n) }
}

#[no_mangle]
pub unsafe extern "C" fn strcpy(dest: *mut c_char, src: *const c_char) -> *mut c_char {
    unsafe { strcpy_internal(dest, src) }
}

#[no_mangle]
pub unsafe extern "C" fn strncpy(dest: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
    unsafe { strncpy_internal(dest, src, n) }
}

#[no_mangle]
pub extern "C" fn isspace_k(c: c_int) -> c_int {
    from_bool(isspace(c as u8))
}

#[no_mangle]
pub extern "C" fn isdigit_k(c: c_int) -> c_int {
    from_bool(isdigit(c as u8))
}

#[no_mangle]
pub extern "C" fn tolower_k(c: c_int) -> c_int {
    tolower(c as u8) as c_int
}

#[no_mangle]
pub extern "C" fn toupper_k(c: c_int) -> c_int {
    toupper(c as u8) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn strcasecmp(lhs: *const c_char, rhs: *const c_char) -> c_int {
    unsafe { strcasecmp_internal(lhs, rhs) }
}

#[no_mangle]
pub unsafe extern "C" fn strncasecmp(lhs: *const c_char, rhs: *const c_char, n: usize) -> c_int {
    unsafe { strncasecmp_internal(lhs, rhs, n) }
}

#[no_mangle]
pub unsafe extern "C" fn strchr(str: *const c_char, c: c_int) -> *mut c_char {
    unsafe { strchr_internal(str, c) }
}

#[no_mangle]
pub unsafe extern "C" fn strstr(haystack: *const c_char, needle: *const c_char) -> *mut c_char {
    unsafe { strstr_internal(haystack, needle) }
}

#[no_mangle]
pub unsafe extern "C" fn str_has_token(str: *const c_char, token: *const c_char) -> c_int {
    unsafe { str_has_token_internal(str, token) }
}


