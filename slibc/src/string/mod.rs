use core::ffi::c_void;

pub mod convert;

pub fn u_memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    if dst.is_null() || src.is_null() || n == 0 {
        return dst;
    }
    unsafe {
        let mut d = dst as *mut u8;
        let mut s = src as *const u8;
        for _ in 0..n {
            *d = *s;
            d = d.add(1);
            s = s.add(1);
        }
    }
    dst
}

pub fn u_memset(dst: *mut c_void, c: i32, n: usize) -> *mut c_void {
    if dst.is_null() || n == 0 {
        return dst;
    }
    unsafe {
        let mut d = dst as *mut u8;
        for _ in 0..n {
            *d = c as u8;
            d = d.add(1);
        }
    }
    dst
}

pub fn u_strlen(s: *const u8) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut len = 0usize;
    unsafe {
        let mut p = s;
        while *p != 0 {
            len += 1;
            p = p.add(1);
        }
    }
    len
}

pub fn u_strnlen(s: *const u8, maxlen: usize) -> usize {
    if s.is_null() || maxlen == 0 {
        return 0;
    }
    let mut len = 0usize;
    unsafe {
        let mut p = s;
        while len < maxlen && *p != 0 {
            len += 1;
            p = p.add(1);
        }
    }
    len
}

#[inline(always)]
pub fn ptr_is_null<T>(ptr: *const T) -> bool {
    ptr.is_null()
}

#[inline(always)]
pub fn slice_from_cstr<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 || ptr.is_null() {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }
}

#[inline(always)]
pub fn slice_from_cstr_mut<'a>(ptr: *mut u8, len: usize) -> &'a mut [u8] {
    if len == 0 || ptr.is_null() {
        &mut []
    } else {
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    if dst.is_null() || src.is_null() || n == 0 {
        return dst;
    }
    let dst = dst as *mut u8;
    let src = src as *const u8;
    let mut i = 0usize;
    while i < n {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
    dst as *mut c_void
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    if dst.is_null() || src.is_null() || n == 0 {
        return dst;
    }
    let dst = dst as *mut u8;
    let src = src as *const u8;
    let d = dst as usize;
    let s = src as usize;
    if d > s && d < s.saturating_add(n) {
        let mut i = n;
        while i > 0 {
            i -= 1;
            *dst.add(i) = *src.add(i);
        }
    } else {
        let mut i = 0usize;
        while i < n {
            *dst.add(i) = *src.add(i);
            i += 1;
        }
    }
    dst as *mut c_void
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dst: *mut c_void, c: i32, n: usize) -> *mut c_void {
    if dst.is_null() || n == 0 {
        return dst;
    }
    let dst = dst as *mut u8;
    let mut i = 0usize;
    while i < n {
        *dst.add(i) = c as u8;
        i += 1;
    }
    dst as *mut c_void
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    let a = a as *const u8;
    let b = b as *const u8;
    let mut i = 0usize;
    while i < n {
        let av = *a.add(i);
        let bv = *b.add(i);
        if av != bv {
            return av as i32 - bv as i32;
        }
        i += 1;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memchr(s: *const u8, c: i32, n: usize) -> *const u8 {
    if s.is_null() || n == 0 {
        return core::ptr::null();
    }
    let target = c as u8;
    let mut i = 0usize;
    while i < n {
        if *s.add(i) == target {
            return s.add(i);
        }
        i += 1;
    }
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strlen(s: *const i8) -> usize {
    u_strlen(s.cast())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strnlen(s: *const u8, maxlen: usize) -> usize {
    u_strnlen(s, maxlen)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strcpy(dst: *mut u8, src: *const u8) -> *mut u8 {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    let mut i = 0usize;
    loop {
        let ch = *src.add(i);
        *dst.add(i) = ch;
        i += 1;
        if ch == 0 {
            break;
        }
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strncpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if dst.is_null() || src.is_null() || n == 0 {
        return dst;
    }
    let mut i = 0usize;
    while i < n {
        let ch = *src.add(i);
        *dst.add(i) = ch;
        i += 1;
        if ch == 0 {
            break;
        }
    }
    while i < n {
        *dst.add(i) = 0;
        i += 1;
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    let mut i = 0usize;
    loop {
        let av = *a.add(i);
        let bv = *b.add(i);
        if av != bv {
            return av as i32 - bv as i32;
        }
        if av == 0 {
            return 0;
        }
        i += 1;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strncmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    let mut i = 0usize;
    while i < n {
        let av = *a.add(i);
        let bv = *b.add(i);
        if av != bv {
            return av as i32 - bv as i32;
        }
        if av == 0 {
            return 0;
        }
        i += 1;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strchr(s: *const u8, c: i32) -> *const u8 {
    if s.is_null() {
        return core::ptr::null();
    }
    let target = c as u8;
    let mut i = 0usize;
    loop {
        let ch = *s.add(i);
        if ch == target {
            return s.add(i);
        }
        if ch == 0 {
            return core::ptr::null();
        }
        i += 1;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strrchr(s: *const u8, c: i32) -> *const u8 {
    if s.is_null() {
        return core::ptr::null();
    }
    let target = c as u8;
    let mut last = core::ptr::null();
    let mut i = 0usize;
    loop {
        let ch = *s.add(i);
        if ch == target {
            last = s.add(i);
        }
        if ch == 0 {
            break;
        }
        i += 1;
    }
    last
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strstr(haystack: *const u8, needle: *const u8) -> *const u8 {
    if haystack.is_null() || needle.is_null() {
        return core::ptr::null();
    }
    if *needle == 0 {
        return haystack;
    }
    let mut i = 0usize;
    while *haystack.add(i) != 0 {
        let mut j = 0usize;
        loop {
            let nv = *needle.add(j);
            if nv == 0 {
                return haystack.add(i);
            }
            let hv = *haystack.add(i + j);
            if hv == 0 {
                return core::ptr::null();
            }
            if hv != nv {
                break;
            }
            j += 1;
        }
        i += 1;
    }
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strcat(dst: *mut u8, src: *const u8) -> *mut u8 {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    let mut dlen = 0usize;
    while *dst.add(dlen) != 0 {
        dlen += 1;
    }
    let mut i = 0usize;
    loop {
        let ch = *src.add(i);
        *dst.add(dlen + i) = ch;
        i += 1;
        if ch == 0 {
            break;
        }
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strncat(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    let mut dlen = 0usize;
    while *dst.add(dlen) != 0 {
        dlen += 1;
    }
    let mut i = 0usize;
    while i < n {
        let ch = *src.add(i);
        if ch == 0 {
            break;
        }
        *dst.add(dlen + i) = ch;
        i += 1;
    }
    *dst.add(dlen + i) = 0;
    dst
}
