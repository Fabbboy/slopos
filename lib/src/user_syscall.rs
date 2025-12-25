use core::arch::asm;
use core::ffi::{c_char, c_int, c_void};
use core::hint::unreachable_unchecked;

use crate::syscall_numbers::*;
use crate::user_syscall_defs::*;

#[inline(always)]
pub unsafe fn syscall_invoke(num: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") num,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            lateout("rax") ret,
            options(nostack, preserves_flags),
        );
    }
    ret
}

#[inline(always)]
fn invoke(num: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { syscall_invoke(num, arg0, arg1, arg2) }
}
pub extern "C" fn sys_yield() -> i64 {
    invoke(SYSCALL_YIELD, 0, 0, 0)
}
pub extern "C" fn sys_exit() -> ! {
    invoke(SYSCALL_EXIT, 0, 0, 0);
    unsafe { unreachable_unchecked() }
}
pub extern "C" fn sys_write(buf: *const c_void, len: usize) -> i64 {
    invoke(SYSCALL_WRITE, buf as u64, len as u64, 0)
}
pub extern "C" fn sys_read(buf: *mut c_void, len: usize) -> i64 {
    invoke(SYSCALL_READ, buf as u64, len as u64, 0)
}
pub extern "C" fn sys_roulette() -> u64 {
    invoke(SYSCALL_ROULETTE, 0, 0, 0) as u64
}
pub extern "C" fn sys_sleep_ms(ms: u64) -> i64 {
    invoke(SYSCALL_SLEEP_MS, ms, 0, 0)
}
pub extern "C" fn sys_fb_info(out_info: *mut user_fb_info) -> i64 {
    invoke(SYSCALL_FB_INFO, out_info as u64, 0, 0)
}
pub extern "C" fn sys_random_next() -> u32 {
    invoke(SYSCALL_RANDOM_NEXT, 0, 0, 0) as u32
}
pub extern "C" fn sys_roulette_result(fate_packed: u64) -> i64 {
    invoke(SYSCALL_ROULETTE_RESULT, fate_packed, 0, 0)
}
pub extern "C" fn sys_fs_open(path: *const c_char, flags: u32) -> i64 {
    invoke(SYSCALL_FS_OPEN, path as u64, flags as u64, 0)
}
pub extern "C" fn sys_fs_close(fd: c_int) -> i64 {
    invoke(SYSCALL_FS_CLOSE, fd as u64, 0, 0)
}
pub extern "C" fn sys_fs_read(fd: c_int, buf: *mut c_void, len: usize) -> i64 {
    invoke(SYSCALL_FS_READ, fd as u64, buf as u64, len as u64)
}
pub extern "C" fn sys_fs_write(fd: c_int, buf: *const c_void, len: usize) -> i64 {
    invoke(SYSCALL_FS_WRITE, fd as u64, buf as u64, len as u64)
}
pub extern "C" fn sys_fs_stat(path: *const c_char, out_stat: *mut user_fs_stat) -> i64 {
    invoke(SYSCALL_FS_STAT, path as u64, out_stat as u64, 0)
}
pub extern "C" fn sys_fs_mkdir(path: *const c_char) -> i64 {
    invoke(SYSCALL_FS_MKDIR, path as u64, 0, 0)
}
pub extern "C" fn sys_fs_unlink(path: *const c_char) -> i64 {
    invoke(SYSCALL_FS_UNLINK, path as u64, 0, 0)
}
pub extern "C" fn sys_fs_list(path: *const c_char, list: *mut user_fs_list) -> i64 {
    invoke(SYSCALL_FS_LIST, path as u64, list as u64, 0)
}
pub extern "C" fn sys_sys_info(info: *mut user_sys_info) -> i64 {
    invoke(SYSCALL_SYS_INFO, info as u64, 0, 0)
}
pub extern "C" fn sys_halt() -> ! {
    invoke(SYSCALL_HALT, 0, 0, 0);
    unsafe { unreachable_unchecked() }
}
