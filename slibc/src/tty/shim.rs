//! Safe wrappers over `tty::*` for use from tests.

use slopos_abi::syscall::UserTermios;

pub fn cfmakeraw(t: &mut UserTermios) {
    // SAFETY: `t` is a live `&mut UserTermios`; pointer is non-null,
    // aligned, valid for one UserTermios write.
    unsafe { super::cfmakeraw(t as *mut UserTermios) }
}

pub fn cfgetispeed(t: &UserTermios) -> u32 {
    // SAFETY: `t` is a live `&UserTermios`; pointer is non-null, valid
    // for one UserTermios read.
    unsafe { super::cfgetispeed(t as *const UserTermios) }
}

pub fn cfgetospeed(t: &UserTermios) -> u32 {
    // SAFETY: `t` is a live `&UserTermios`; pointer is non-null, valid
    // for one UserTermios read.
    unsafe { super::cfgetospeed(t as *const UserTermios) }
}

pub fn cfsetispeed(t: &mut UserTermios, speed: u32) -> i32 {
    // SAFETY: `t` is a live `&mut UserTermios`.
    unsafe { super::cfsetispeed(t as *mut UserTermios, speed) }
}

pub fn cfsetospeed(t: &mut UserTermios, speed: u32) -> i32 {
    // SAFETY: `t` is a live `&mut UserTermios`.
    unsafe { super::cfsetospeed(t as *mut UserTermios, speed) }
}

pub fn cfgetispeed_null() -> u32 {
    // SAFETY: null `termios` is documented as a defined input that
    // returns 0.
    unsafe { super::cfgetispeed(core::ptr::null()) }
}

pub fn cfsetispeed_null(speed: u32) -> i32 {
    // SAFETY: null `termios` is documented as a defined input that
    // returns -1.
    unsafe { super::cfsetispeed(core::ptr::null_mut(), speed) }
}

pub fn tcgetattr_null(fd: i32) -> i32 {
    // SAFETY: null `termios` is documented as a defined input that
    // returns -1.
    unsafe { super::tcgetattr(fd, core::ptr::null_mut()) }
}

pub fn tcsetattr_null(fd: i32, action: i32) -> i32 {
    // SAFETY: null `termios` is documented as a defined input that
    // returns -1.
    unsafe { super::tcsetattr(fd, action, core::ptr::null()) }
}

pub fn tcsetattr(fd: i32, action: i32, t: &UserTermios) -> i32 {
    // SAFETY: `t` is a live `&UserTermios`.
    unsafe { super::tcsetattr(fd, action, t as *const UserTermios) }
}
