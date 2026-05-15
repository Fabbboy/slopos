//! Terminal I/O — ioctl, termios, and the sacred runes of raw mode.
//! The terminal is the gateway between the wizards and the Wheel.

#[allow(dead_code)]
pub(crate) mod shim;
pub mod tests;

use crate::errno::errno_set;
use crate::pal::{Pal, Sys};
use slopos_abi::syscall::{
    InputFlags, LocalFlags, OutputFlags, TCGETS, TCSETS, TCSETSF, TCSETSW, UserTermios, VMIN, VTIME,
};

// =============================================================================
// tcsetattr optional_actions constants
// =============================================================================

pub const TCSANOW: i32 = 0;
pub const TCSADRAIN: i32 = 1;
pub const TCSAFLUSH: i32 = 2;

// =============================================================================
// ioctl
// =============================================================================

/// Perform device-specific I/O control.
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

// =============================================================================
// termios functions
// =============================================================================

/// Get the parameters associated with the terminal.
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

/// Set the parameters associated with the terminal.
///
/// `optional_actions` determines when the change takes effect:
/// - `TCSANOW`   (0): immediately
/// - `TCSADRAIN` (1): after all output written
/// - `TCSAFLUSH` (2): after output written, discard pending input
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

/// Put terminal into raw mode — disable canonical processing, echo, signals.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfmakeraw(termios: *mut UserTermios) {
    if termios.is_null() {
        return;
    }

    // Clear input flags
    (*termios).c_iflag &= !(InputFlags::IGNBRK
        | InputFlags::INPCK
        | InputFlags::ISTRIP
        | InputFlags::INPCK
        | InputFlags::ICRNL
        | InputFlags::IXON);
    // Clear output flags
    (*termios).c_oflag &= !OutputFlags::OPOST;
    // Clear local flags
    (*termios).c_lflag &= !(LocalFlags::ECHO
        | LocalFlags::ECHOE
        | LocalFlags::ECHOK
        | LocalFlags::ECHONL
        | LocalFlags::ICANON
        | LocalFlags::ISIG
        | LocalFlags::IEXTEN);
    // Set minimum input: 1 byte, no timeout
    (*termios).c_cc[VMIN] = 1;
    (*termios).c_cc[VTIME] = 0;
}

/// Get input baud rate from termios.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfgetispeed(termios: *const UserTermios) -> u32 {
    if termios.is_null() {
        return 0;
    }
    (*termios).c_ispeed
}

/// Set input baud rate in termios.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfsetispeed(termios: *mut UserTermios, speed: u32) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    (*termios).c_ispeed = speed;
    0
}

/// Get output baud rate from termios.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfgetospeed(termios: *const UserTermios) -> u32 {
    if termios.is_null() {
        return 0;
    }
    (*termios).c_ospeed
}

/// Set output baud rate in termios.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfsetospeed(termios: *mut UserTermios, speed: u32) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    (*termios).c_ospeed = speed;
    0
}
