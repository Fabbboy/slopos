use slopos_abi::InputEvent;

slopos_service_core::define_service! {
    input => InputServices {
        poll(task_id: u32) -> Option<InputEvent>;
        drain_batch(task_id: u32, buffer: *mut InputEvent, max_count: usize) -> usize;
        event_count(task_id: u32) -> u32;
        set_keyboard_focus(task_id: u32);
        set_pointer_focus(task_id: u32, timestamp_ms: u64);
        set_pointer_focus_with_offset(task_id: u32, x: i32, y: i32, timestamp_ms: u64);
        request_close(task_id: u32, timestamp_ms: u64) -> bool;
        send_configure(task_id: u32, width: u32, height: u32, timestamp_ms: u64) -> bool;
        get_pointer_focus() -> u32;
        get_pointer_position() -> (i32, i32);
        get_button_state() -> u8;
        get_modifier_state() -> u8;
        clipboard_copy(src: &[u8]) -> usize;
        clipboard_paste(dst: &mut [u8]) -> usize;
        register_compositor(task_id: u32);
    }
}
