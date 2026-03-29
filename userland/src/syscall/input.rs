//! Input event syscalls.

use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall2, syscall3};
use slopos_abi::{INPUT_FOCUS_KEYBOARD, INPUT_FOCUS_POINTER, InputEvent};

pub fn poll(event_out: &mut InputEvent) -> Option<InputEvent> {
    let result = unsafe { syscall1(SYSCALL_INPUT_POLL, event_out as *mut InputEvent as u64) };
    if result == 1 { Some(*event_out) } else { None }
}

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

pub fn has_events() -> u32 {
    unsafe { syscall0(SYSCALL_INPUT_HAS_EVENTS) as u32 }
}

pub fn set_focus(target_task_id: u32, focus_type: u32) -> i64 {
    unsafe {
        syscall2(
            SYSCALL_INPUT_SET_FOCUS,
            target_task_id as u64,
            focus_type as u64,
        ) as i64
    }
}

pub fn set_keyboard_focus(target_task_id: u32) -> i64 {
    set_focus(target_task_id, INPUT_FOCUS_KEYBOARD)
}

pub fn set_pointer_focus(target_task_id: u32) -> i64 {
    set_focus(target_task_id, INPUT_FOCUS_POINTER)
}

pub fn set_pointer_focus_with_offset(target_task_id: u32, offset_x: i32, offset_y: i32) -> i64 {
    unsafe {
        syscall3(
            SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET,
            target_task_id as u64,
            offset_x as u64,
            offset_y as u64,
        ) as i64
    }
}

pub fn request_close(target_task_id: u32) -> i64 {
    unsafe { syscall1(SYSCALL_INPUT_REQUEST_CLOSE, target_task_id as u64) as i64 }
}

pub fn send_configure(target_task_id: u32, width: u32, height: u32) -> i64 {
    unsafe {
        syscall3(
            SYSCALL_SEND_CONFIGURE,
            target_task_id as u64,
            width as u64,
            height as u64,
        ) as i64
    }
}

pub fn get_pointer_pos() -> (i32, i32) {
    let result = unsafe { syscall0(SYSCALL_INPUT_GET_POINTER_POS) };
    let x = (result >> 32) as i32;
    let y = result as i32;
    (x, y)
}

pub fn get_button_state() -> u8 {
    unsafe { syscall0(SYSCALL_INPUT_GET_BUTTON_STATE) as u8 }
}

pub fn get_modifier_state() -> u8 {
    unsafe { syscall0(SYSCALL_INPUT_GET_MODIFIER_STATE) as u8 }
}

#[inline(always)]
pub fn drain_queue() {
    unsafe {
        syscall0(SYSCALL_DRAIN_QUEUE);
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
