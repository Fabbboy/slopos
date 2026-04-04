//! Memory management syscalls: brk, sbrk, shared memory.

use core::ffi::c_void;

use super::numbers::*;
use super::raw::{syscall1, syscall2};

#[inline(always)]
pub fn brk(addr: *mut c_void) -> *mut c_void {
    unsafe { syscall1(SYSCALL_BRK, addr as u64) as *mut c_void }
}

#[inline(always)]
pub fn sbrk(increment: isize) -> *mut c_void {
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

use super::raw::syscall6;

// ---------------------------------------------------------------------------
// memfd + mmap wrappers (fd-based shared memory)
// ---------------------------------------------------------------------------

#[inline(always)]
pub fn memfd_create(flags: u32) -> i32 {
    unsafe { syscall1(SYSCALL_MEMFD_CREATE, flags as u64) as i32 }
}

#[inline(always)]
pub fn ftruncate(fd: i32, size: u64) -> i32 {
    unsafe { syscall2(SYSCALL_FTRUNCATE, fd as u64, size) as i32 }
}

#[inline(always)]
pub fn mmap(addr: u64, length: u64, prot: u64, flags: u64, fd: i64, offset: u64) -> u64 {
    unsafe { syscall6(SYSCALL_MMAP, addr, length, prot, flags, fd as u64, offset) }
}

#[inline(always)]
pub fn munmap(addr: u64, length: u64) -> i32 {
    unsafe { syscall2(SYSCALL_MUNMAP, addr, length) as i32 }
}

#[inline(always)]
pub fn close(fd: i32) -> i32 {
    unsafe { syscall1(SYSCALL_FS_CLOSE, fd as u64) as i32 }
}
