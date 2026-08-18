//! Terminal I/O — ioctl, termios, raw mode.

#[allow(dead_code)]
pub(crate) mod shim;
pub mod tests;

use crate::errno::errno_set;
use crate::pal::{Pal, Sys};
use slopos_abi::syscall::{
    InputFlags, LocalFlags, OutputFlags, TCGETS, TCSETS, TCSETSF, TCSETSW, UserTermios, VMIN, VTIME,
};

pub const TCSANOW: i32 = 0;
pub const TCSADRAIN: i32 = 1;
pub const TCSAFLUSH: i32 = 2;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ioctl(fd: i32, request: u64, arg: u64) -> i32 {
    match Sys::ioctl(fd, request, arg) {
        Ok(ret) => ret,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tcgetattr(fd: i32, termios: *mut UserTermios) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    match Sys::ioctl(fd, TCGETS, termios as u64) {
        Ok(_) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `optional_actions` selects when the change takes effect: `TCSANOW`
/// immediately, `TCSADRAIN` after pending output drains, `TCSAFLUSH` after
/// output drains and pending input is discarded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tcsetattr(
    fd: i32,
    optional_actions: i32,
    termios: *const UserTermios,
) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }

    let request = match optional_actions {
        TCSANOW => TCSETS,
        TCSADRAIN => TCSETSW,
        TCSAFLUSH => TCSETSF,
        _ => {
            errno_set(crate::errno::EINVAL.raw());
            return -1;
        }
    };

    match Sys::ioctl(fd, request, termios as u64) {
        Ok(_) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfmakeraw(termios: *mut UserTermios) {
    if termios.is_null() {
        return;
    }

    (*termios).c_iflag &= !(InputFlags::IGNBRK
        | InputFlags::INPCK
        | InputFlags::ISTRIP
        | InputFlags::INPCK
        | InputFlags::ICRNL
        | InputFlags::IXON);
    (*termios).c_oflag &= !OutputFlags::OPOST;
    (*termios).c_lflag &= !(LocalFlags::ECHO
        | LocalFlags::ECHOE
        | LocalFlags::ECHOK
        | LocalFlags::ECHONL
        | LocalFlags::ICANON
        | LocalFlags::ISIG
        | LocalFlags::IEXTEN);
    (*termios).c_cc[VMIN] = 1;
    (*termios).c_cc[VTIME] = 0;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfgetispeed(termios: *const UserTermios) -> u32 {
    if termios.is_null() {
        return 0;
    }
    (*termios).c_ispeed
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfsetispeed(termios: *mut UserTermios, speed: u32) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    (*termios).c_ispeed = speed;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfgetospeed(termios: *const UserTermios) -> u32 {
    if termios.is_null() {
        return 0;
    }
    (*termios).c_ospeed
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfsetospeed(termios: *mut UserTermios, speed: u32) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    (*termios).c_ospeed = speed;
    0
}
