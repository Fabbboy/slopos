use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use crate::syscall_types::{InterruptFrame, Task};

pub const USER_IO_MAX_BYTES: usize = 512;
pub const USER_PATH_MAX: usize = 128;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SyscallDisposition {
    Ok = 0,
    NoReturn = 1,
}

pub type SyscallHandler = fn(*mut Task, *mut InterruptFrame) -> SyscallDisposition;

#[repr(C)]
pub struct SyscallEntry {
    pub handler: Option<SyscallHandler>,
    pub name: *const c_char,
}

use slopos_mm::user_copy::{user_copy_from_user, user_copy_to_user};

pub fn syscall_return_ok(frame: *mut InterruptFrame, value: u64) -> SyscallDisposition {
    if frame.is_null() {
        return SyscallDisposition::Ok;
    }
    unsafe {
        (*frame).rax = value;
    }
    SyscallDisposition::Ok
}

pub fn syscall_return_err(frame: *mut InterruptFrame, _err_value: u64) -> SyscallDisposition {
    if frame.is_null() {
        return SyscallDisposition::Ok;
    }
    unsafe {
        (*frame).rax = u64::MAX;
    }
    SyscallDisposition::Ok
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
    user_copy_to_user(user_dst, src, len)
}
