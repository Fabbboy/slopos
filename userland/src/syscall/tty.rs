//! TTY/Console I/O syscalls (NOT file descriptor operations).

use super::numbers::{SYSCALL_READ, SYSCALL_WRITE};
use super::raw::{syscall2, syscall5};
use slopos_abi::syscall::SYSCALL_FONT_SET;

#[inline(always)]
pub fn write(buf: &[u8]) -> i64 {
    unsafe { syscall2(SYSCALL_WRITE, buf.as_ptr() as u64, buf.len() as u64) as i64 }
}

#[inline(always)]
pub fn read(buf: &mut [u8]) -> i64 {
    unsafe { syscall2(SYSCALL_READ, buf.as_ptr() as u64, buf.len() as u64) as i64 }
}

#[inline(always)]
pub fn font_set_coverage(data: &[u8], width: u16, height: u16) -> i64 {
    unsafe {
        syscall5(
            SYSCALL_FONT_SET,
            data.as_ptr() as u64,
            width as u64,
            height as u64,
            slopos_font::ASCII_COUNT as u64,
            1,
        ) as i64
    }
}
