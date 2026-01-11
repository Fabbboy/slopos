use core::ffi::{c_char, c_int};

use slopos_abi::task::Task;
use slopos_lib::InterruptFrame;

use slopos_mm::user_copy::{copy_bytes_from_user, copy_bytes_to_user};
use slopos_mm::user_ptr::{UserBytes, UserPtrError};

pub const USER_IO_MAX_BYTES: usize = 512;
pub use slopos_abi::fs::USER_PATH_MAX;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SyscallDisposition {
    Ok = 0,
    NoReturn = 1,
}

pub type SyscallHandler = fn(*mut Task, *mut InterruptFrame) -> SyscallDisposition;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct SyscallEntry {
    pub handler: Option<SyscallHandler>,
    pub name: *const c_char,
}

unsafe impl Sync for SyscallEntry {}

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

pub fn syscall_copy_user_str(dst: &mut [u8], user_src: u64) -> Result<(), UserPtrError> {
    if dst.is_empty() {
        return Err(UserPtrError::Null);
    }

    let cap = dst.len().saturating_sub(1);
    let user_bytes = UserBytes::try_new(user_src, cap)?;
    copy_bytes_from_user(user_bytes, &mut dst[..cap])?;

    dst[cap] = 0;
    for i in 0..cap {
        if dst[i] == 0 {
            return Ok(());
        }
    }
    dst[cap] = 0;
    Ok(())
}

pub fn syscall_copy_user_str_to_cstr(dst: &mut [i8], user_src: u64) -> c_int {
    let dst_u8 = unsafe { core::slice::from_raw_parts_mut(dst.as_mut_ptr() as *mut u8, dst.len()) };
    match syscall_copy_user_str(dst_u8, user_src) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

pub fn syscall_bounded_from_user(
    dst: &mut [u8],
    user_src: u64,
    requested_len: u64,
    cap_len: usize,
) -> Result<usize, UserPtrError> {
    if dst.is_empty() || requested_len == 0 {
        return Err(UserPtrError::Null);
    }

    let mut len = requested_len as usize;
    if len > cap_len {
        len = cap_len;
    }
    if len > dst.len() {
        len = dst.len();
    }

    let user_bytes = UserBytes::try_new(user_src, len)?;
    copy_bytes_from_user(user_bytes, &mut dst[..len])?;
    Ok(len)
}

pub fn syscall_copy_to_user_bounded(user_dst: u64, src: &[u8]) -> Result<(), UserPtrError> {
    if src.is_empty() {
        return Ok(());
    }

    let user_bytes = UserBytes::try_new(user_dst, src.len())?;
    copy_bytes_to_user(user_bytes, src)?;
    Ok(())
}
