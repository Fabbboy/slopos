//! Input event syscalls.

use super::numbers::*;
use super::raw::syscall2;
use slopos_abi::InputEvent;

#[inline(always)]
pub fn poll_batch(events: &mut [InputEvent]) -> u64 {
    unsafe {
        syscall2(
            SYSCALL_INPUT_POLL_BATCH,
            events.as_mut_ptr() as u64,
            events.len() as u64,
        )
    }
}

pub fn clipboard_copy(data: &[u8]) -> usize {
    if data.is_empty() {
        return 0;
    }
    unsafe {
        syscall2(
            SYSCALL_CLIPBOARD_COPY,
            data.as_ptr() as u64,
            data.len() as u64,
        ) as usize
    }
}

pub fn clipboard_paste(buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    unsafe {
        syscall2(
            SYSCALL_CLIPBOARD_PASTE,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as usize
    }
}
