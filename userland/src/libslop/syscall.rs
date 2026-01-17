#![allow(unsafe_op_in_unsafe_fn)]

use core::arch::asm;
use core::ffi::{c_char, c_int, c_void};

use slopos_abi::syscall::*;

#[allow(dead_code)]
#[inline(always)]
pub(crate) unsafe fn syscall0(num: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        in("rax") num,
        lateout("rax") ret,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

#[inline(always)]
pub(crate) unsafe fn syscall1(num: u64, arg0: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        in("rax") num,
        in("rdi") arg0,
        lateout("rax") ret,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

#[inline(always)]
pub(crate) unsafe fn syscall2(num: u64, arg0: u64, arg1: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        in("rax") num,
        in("rdi") arg0,
        in("rsi") arg1,
        lateout("rax") ret,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

#[inline(always)]
pub(crate) unsafe fn syscall3(num: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        in("rax") num,
        in("rdi") arg0,
        in("rsi") arg1,
        in("rdx") arg2,
        lateout("rax") ret,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

pub fn sys_read(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    unsafe { syscall3(SYSCALL_FS_READ, fd as u64, buf as u64, count as u64) as isize }
}

pub fn sys_write(fd: c_int, buf: *const c_void, count: usize) -> isize {
    unsafe { syscall3(SYSCALL_FS_WRITE, fd as u64, buf as u64, count as u64) as isize }
}

pub fn sys_open(path: *const c_char, flags: c_int) -> c_int {
    unsafe { syscall2(SYSCALL_FS_OPEN, path as u64, flags as u64) as c_int }
}

pub fn sys_close(fd: c_int) -> c_int {
    unsafe { syscall1(SYSCALL_FS_CLOSE, fd as u64) as c_int }
}

pub fn sys_exit(status: c_int) -> ! {
    unsafe {
        syscall1(SYSCALL_EXIT, status as u64);
    }
    loop {
        core::hint::spin_loop();
    }
}

pub fn sys_brk(addr: *mut c_void) -> *mut c_void {
    unsafe { syscall1(SYSCALL_BRK, addr as u64) as *mut c_void }
}

pub fn sys_sbrk(increment: isize) -> *mut c_void {
    unsafe {
        let current = syscall1(SYSCALL_BRK, 0) as usize;
        if increment == 0 {
            return current as *mut c_void;
        }
        let new_brk = if increment > 0 {
            current.wrapping_add(increment as usize)
        } else {
            current.wrapping_sub((-increment) as usize)
        };
        let result = syscall1(SYSCALL_BRK, new_brk as u64) as usize;
        if result == new_brk {
            current as *mut c_void
        } else {
            usize::MAX as *mut c_void
        }
    }
}
