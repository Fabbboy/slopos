use core::ffi::c_int;
pub unsafe fn memmove_internal(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe {
        if dest as *const u8 == src || n == 0 {
            return dest;
        }

        if dest < src as *mut u8 {
            let mut i = 0usize;
            while i < n {
                *dest.add(i) = *src.add(i);
                i += 1;
            }
        } else {
            let mut i = n;
            while i > 0 {
                i -= 1;
                *dest.add(i) = *src.add(i);
            }
        }

        dest
    }
}

pub unsafe fn memset_internal(dest: *mut u8, value: i32, n: usize) -> *mut u8 {
    unsafe {
        let mut i = 0usize;
        let val = value as u8;
        while i < n {
            *dest.add(i) = val;
            i += 1;
        }
        dest
    }
}

pub unsafe fn memcpy_internal(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe {
        let mut i = 0usize;
        while i < n {
            *dest.add(i) = *src.add(i);
            i += 1;
        }
        dest
    }
}

pub unsafe fn memcmp_internal(s1: *const u8, s2: *const u8, n: usize) -> c_int {
    unsafe {
        let mut i = 0usize;
        while i < n {
            let a = *s1.add(i);
            let b = *s2.add(i);
            if a != b {
                return if a < b { -1 } else { 1 };
            }
            i += 1;
        }
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe { memmove_internal(dest, src, n) }
}

#[unsafe(no_mangle)]
pub extern "C" fn memset(dest: *mut u8, value: i32, n: usize) -> *mut u8 {
    unsafe { memset_internal(dest, value, n) }
}

#[unsafe(no_mangle)]
pub extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe { memcpy_internal(dest, src, n) }
}

#[unsafe(no_mangle)]
pub extern "C" fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> c_int {
    unsafe { memcmp_internal(s1, s2, n) }
}


