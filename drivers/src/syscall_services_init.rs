use slopos_abi::InputEvent;
use slopos_abi::fate::FateResult;

use slopos_core::syscall_services::{
    FateServices, InputServices, TtyServices, register_fate_services, register_input_services,
    register_tty_services,
};

use crate::{fate, input_event, tty};

static INPUT_SERVICES: InputServices = InputServices {
    poll: input_poll,
    drain_batch: input_drain_batch,
    event_count: input_event_count,
    set_keyboard_focus: input_set_keyboard_focus,
    set_pointer_focus: input_set_pointer_focus,
    set_pointer_focus_with_offset: input_set_pointer_focus_with_offset,
    get_pointer_focus: input_get_pointer_focus,
    get_pointer_position: input_get_pointer_position,
    get_button_state: input_get_button_state,
};

fn input_poll(task_id: u32) -> Option<InputEvent> {
    input_event::input_poll(task_id)
}

fn input_drain_batch(task_id: u32, buf: *mut InputEvent, max: usize) -> usize {
    input_event::input_drain_batch(task_id, buf, max)
}

fn input_event_count(task_id: u32) -> usize {
    input_event::input_event_count(task_id) as usize
}

fn input_set_keyboard_focus(task_id: u32) {
    input_event::input_set_keyboard_focus(task_id)
}

fn input_set_pointer_focus(task_id: u32, ts: u64) {
    input_event::input_set_pointer_focus(task_id, ts)
}

fn input_set_pointer_focus_with_offset(task_id: u32, x: i32, y: i32, ts: u64) {
    input_event::input_set_pointer_focus_with_offset(task_id, x, y, ts)
}

fn input_get_pointer_focus() -> u32 {
    input_event::input_get_pointer_focus()
}

fn input_get_pointer_position() -> (i32, i32) {
    input_event::input_get_pointer_position()
}

fn input_get_button_state() -> u32 {
    input_event::input_get_button_state() as u32
}

static TTY_SERVICES: TtyServices = TtyServices {
    read_line: tty_read_line,
    read_char_blocking: tty_read_char_blocking,
    set_focus: tty_set_focus,
    get_focus: tty_get_focus,
};

fn tty_read_line(buf: *mut u8, len: usize) -> usize {
    tty::tty_read_line(buf, len)
}

fn tty_read_char_blocking(buf: *mut u8) -> i32 {
    tty::tty_read_char_blocking(buf)
}

fn tty_set_focus(target: u32) -> i32 {
    tty::tty_set_focus(target)
}

fn tty_get_focus() -> u32 {
    tty::tty_get_focus()
}

static FATE_SERVICES: FateServices = FateServices {
    notify_outcome: fate_notify_outcome,
};

fn fate_notify_outcome(result: *const FateResult) {
    fate::fate_notify_outcome(result)
}

pub fn init_syscall_services() {
    register_input_services(&INPUT_SERVICES);
    register_tty_services(&TTY_SERVICES);
    register_fate_services(&FATE_SERVICES);
}
