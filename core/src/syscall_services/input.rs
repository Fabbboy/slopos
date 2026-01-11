use slopos_abi::InputEvent;
use slopos_lib::ServiceCell;

#[repr(C)]
pub struct InputServices {
    pub poll: fn(u32) -> Option<InputEvent>,
    pub drain_batch: fn(u32, *mut InputEvent, usize) -> usize,
    pub event_count: fn(u32) -> usize,
    pub set_keyboard_focus: fn(u32),
    pub set_pointer_focus: fn(u32, u64),
    pub set_pointer_focus_with_offset: fn(u32, i32, i32, u64),
    pub get_pointer_focus: fn() -> u32,
    pub get_pointer_position: fn() -> (i32, i32),
    pub get_button_state: fn() -> u32,
}

static INPUT: ServiceCell<InputServices> = ServiceCell::new("input");

pub fn register_input_services(services: &'static InputServices) {
    INPUT.register(services);
}

pub fn is_input_initialized() -> bool {
    INPUT.is_initialized()
}

#[inline(always)]
pub fn input_services() -> &'static InputServices {
    INPUT.get()
}

#[inline(always)]
pub fn input_poll(task_id: u32) -> Option<InputEvent> {
    (input_services().poll)(task_id)
}

#[inline(always)]
pub fn input_drain_batch(task_id: u32, buffer: *mut InputEvent, max_count: usize) -> usize {
    (input_services().drain_batch)(task_id, buffer, max_count)
}

#[inline(always)]
pub fn input_event_count(task_id: u32) -> usize {
    (input_services().event_count)(task_id)
}

#[inline(always)]
pub fn input_set_keyboard_focus(task_id: u32) {
    (input_services().set_keyboard_focus)(task_id)
}

#[inline(always)]
pub fn input_set_pointer_focus(task_id: u32, timestamp_ms: u64) {
    (input_services().set_pointer_focus)(task_id, timestamp_ms)
}

#[inline(always)]
pub fn input_set_pointer_focus_with_offset(task_id: u32, x: i32, y: i32, timestamp_ms: u64) {
    (input_services().set_pointer_focus_with_offset)(task_id, x, y, timestamp_ms)
}

#[inline(always)]
pub fn input_get_pointer_focus() -> u32 {
    (input_services().get_pointer_focus)()
}

#[inline(always)]
pub fn input_get_pointer_position() -> (i32, i32) {
    (input_services().get_pointer_position)()
}

#[inline(always)]
pub fn input_get_button_state() -> u32 {
    (input_services().get_button_state)()
}
