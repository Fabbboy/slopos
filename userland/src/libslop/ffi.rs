#![allow(unsafe_op_in_unsafe_fn)]
#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int, c_void};

use super::malloc as heap;
use super::syscall as sys;

pub type ssize_t = isize;
pub type size_t = usize;

#[unsafe(no_mangle)]
pub extern "C" fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t {
    sys::sys_read(fd, buf, count)
}

#[unsafe(no_mangle)]
pub extern "C" fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t {
    sys::sys_write(fd, buf, count)
}

#[unsafe(no_mangle)]
pub extern "C" fn open(path: *const c_char, flags: c_int) -> c_int {
    sys::sys_open(path, flags)
}

#[unsafe(no_mangle)]
pub extern "C" fn close(fd: c_int) -> c_int {
    sys::sys_close(fd)
}

#[unsafe(no_mangle)]
pub extern "C" fn exit(status: c_int) -> ! {
    sys::sys_exit(status)
}

#[unsafe(no_mangle)]
pub extern "C" fn _exit(status: c_int) -> ! {
    sys::sys_exit(status)
}

#[unsafe(no_mangle)]
pub extern "C" fn brk(addr: *mut c_void) -> *mut c_void {
    sys::sys_brk(addr)
}

#[unsafe(no_mangle)]
pub extern "C" fn sbrk(increment: isize) -> *mut c_void {
    sys::sys_sbrk(increment)
}

#[unsafe(no_mangle)]
pub extern "C" fn malloc(size: size_t) -> *mut c_void {
    heap::alloc(size)
}

#[unsafe(no_mangle)]
pub extern "C" fn free(ptr: *mut c_void) {
    heap::dealloc(ptr)
}

#[unsafe(no_mangle)]
pub extern "C" fn realloc(ptr: *mut c_void, size: size_t) -> *mut c_void {
    heap::realloc(ptr, size)
}

#[unsafe(no_mangle)]
pub extern "C" fn calloc(nmemb: size_t, size: size_t) -> *mut c_void {
    heap::calloc(nmemb, size)
}

pub const O_RDONLY: c_int = 0;
pub const O_WRONLY: c_int = 1;
pub const O_RDWR: c_int = 2;
pub const O_CREAT: c_int = 0x40;
pub const O_TRUNC: c_int = 0x200;
pub const O_APPEND: c_int = 0x400;
