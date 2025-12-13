#![allow(dead_code)]
#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use crate::syscall_types::{task_t, InterruptFrame};

pub const USER_IO_MAX_BYTES: usize = 512;
pub const USER_PATH_MAX: usize = 128;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum syscall_disposition {
    SYSCALL_DISP_OK = 0,
    SYSCALL_DISP_NO_RETURN = 1,
}

pub type syscall_handler_t =
    extern "C" fn(*mut task_t, *mut InterruptFrame) -> syscall_disposition;

#[repr(C)]
pub struct syscall_entry {
    pub handler: Option<syscall_handler_t>,
    pub name: *const c_char,
}

extern "C" {
    fn user_copy_from_user(dst: *mut c_void, src: *const c_void, len: usize) -> c_int;
    fn user_copy_to_user(dst: *mut c_void, src: *const c_void, len: usize) -> c_int;
}

pub fn syscall_return_ok(frame: *mut InterruptFrame, value: u64) -> syscall_disposition {
    if frame.is_null() {
        return syscall_disposition::SYSCALL_DISP_OK;
    }
    unsafe {
        (*frame).rax = value;
    }
    syscall_disposition::SYSCALL_DISP_OK
}

pub fn syscall_return_err(frame: *mut InterruptFrame, _err_value: u64) -> syscall_disposition {
    if frame.is_null() {
        return syscall_disposition::SYSCALL_DISP_OK;
    }
    unsafe {
        (*frame).rax = u64::MAX;
    }
    syscall_disposition::SYSCALL_DISP_OK
}

pub fn syscall_copy_user_str(dst: *mut c_char, dst_len: usize, user_src: *const c_char) -> c_int {
    if dst.is_null() || dst_len == 0 || user_src.is_null() {
        return -1;
    }
    let cap = dst_len.saturating_sub(1);
    let copy_len = cap;
    unsafe {
        if user_copy_from_user(dst as *mut c_void, user_src as *const c_void, copy_len) != 0 {
            return -1;
        }
        let dst_bytes = core::slice::from_raw_parts_mut(dst as *mut u8, dst_len);
        dst_bytes[cap] = 0;
        for i in 0..cap {
            if dst_bytes[i] == 0 {
                return 0;
            }
        }
        dst_bytes[cap] = 0;
    }
    0
}

#[allow(clippy::too_many_arguments)]
pub fn syscall_bounded_from_user(
    dst: *mut c_void,
    dst_len: usize,
    user_src: *const c_void,
    requested_len: u64,
    cap_len: usize,
    copied_len_out: *mut usize,
) -> c_int {
    if dst.is_null() || dst_len == 0 || user_src.is_null() || requested_len == 0 {
        return -1;
    }

    let mut len = requested_len as usize;
    if len > cap_len {
        len = cap_len;
    }
    if len > dst_len {
        len = dst_len;
    }

    unsafe {
        if user_copy_from_user(dst, user_src, len) != 0 {
            return -1;
        }
        if !copied_len_out.is_null() {
            ptr::write(copied_len_out, len);
        }
    }
    0
}

pub fn syscall_copy_to_user_bounded(
    user_dst: *mut c_void,
    src: *const c_void,
    len: usize,
) -> c_int {
    if user_dst.is_null() || src.is_null() || len == 0 {
        return -1;
    }
    unsafe { user_copy_to_user(user_dst, src, len) }
}
