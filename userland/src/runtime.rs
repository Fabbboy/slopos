#![allow(dead_code)]

use core::ffi::c_void;
#[unsafe(no_mangle)]
#[unsafe(link_section = ".user_text")]
pub extern "C" fn u_memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
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

#[unsafe(no_mangle)]
#[unsafe(link_section = ".user_text")]
pub extern "C" fn u_memset(dst: *mut c_void, c: i32, n: usize) -> *mut c_void {
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

#[unsafe(no_mangle)]
#[unsafe(link_section = ".user_text")]
pub extern "C" fn u_strlen(s: *const u8) -> usize {
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

#[unsafe(no_mangle)]
#[unsafe(link_section = ".user_text")]
pub extern "C" fn u_strnlen(s: *const u8, maxlen: usize) -> usize {
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

