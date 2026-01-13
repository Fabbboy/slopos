use slopos_abi::InputEvent;

slopos_lib::define_service! {
    input => InputServices {
        poll(task_id: u32) -> Option<InputEvent>;
        drain_batch(task_id: u32, buffer: *mut InputEvent, max_count: usize) -> usize;
        event_count(task_id: u32) -> usize;
        set_keyboard_focus(task_id: u32);
        set_pointer_focus(task_id: u32, timestamp_ms: u64);
        set_pointer_focus_with_offset(task_id: u32, x: i32, y: i32, timestamp_ms: u64);
        get_pointer_focus() -> u32;
        get_pointer_position() -> (i32, i32);
        get_button_state() -> u32;
    }
}

#[inline(always)]
pub fn input_poll(task_id: u32) -> Option<InputEvent> {
    poll(task_id)
}

#[inline(always)]
pub fn input_drain_batch(task_id: u32, buffer: *mut InputEvent, max_count: usize) -> usize {
    drain_batch(task_id, buffer, max_count)
}

#[inline(always)]
pub fn input_event_count(task_id: u32) -> usize {
    event_count(task_id)
}

#[inline(always)]
pub fn input_set_keyboard_focus(task_id: u32) {
    set_keyboard_focus(task_id)
}

#[inline(always)]
pub fn input_set_pointer_focus(task_id: u32, timestamp_ms: u64) {
    set_pointer_focus(task_id, timestamp_ms)
}

#[inline(always)]
pub fn input_set_pointer_focus_with_offset(task_id: u32, x: i32, y: i32, timestamp_ms: u64) {
    set_pointer_focus_with_offset(task_id, x, y, timestamp_ms)
}

#[inline(always)]
pub fn input_get_pointer_focus() -> u32 {
    get_pointer_focus()
}

#[inline(always)]
pub fn input_get_pointer_position() -> (i32, i32) {
    get_pointer_position()
}

#[inline(always)]
pub fn input_get_button_state() -> u32 {
    get_button_state()
}
