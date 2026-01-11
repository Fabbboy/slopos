use slopos_abi::InputEvent;
use slopos_abi::WindowInfo;
use slopos_abi::addr::PhysAddr;
use slopos_abi::fate::FateResult;
use slopos_abi::video_traits::FramebufferInfoC;

use slopos_core::syscall_services::{
    FateServices, InputServices, TtyServices, VideoServices, register_fate_services,
    register_input_services, register_tty_services, register_video_services,
};

use crate::{fate, input_event, tty, video_bridge};

static VIDEO_SERVICES: VideoServices = VideoServices {
    framebuffer_get_info: video_framebuffer_get_info,
    roulette_draw: video_roulette_draw,
    surface_enumerate_windows: video_surface_enumerate_windows,
    surface_set_window_position: video_surface_set_window_position,
    surface_set_window_state: video_surface_set_window_state,
    surface_raise_window: video_surface_raise_window,
    surface_commit: video_surface_commit,
    register_surface: video_register_surface,
    drain_queue: video_drain_queue,
    fb_flip: video_fb_flip,
    surface_request_frame_callback: video_surface_request_frame_callback,
    surface_mark_frames_done: video_surface_mark_frames_done,
    surface_poll_frame_done: video_surface_poll_frame_done,
    surface_add_damage: video_surface_add_damage,
    surface_get_buffer_age: video_surface_get_buffer_age,
    surface_set_role: video_surface_set_role,
    surface_set_parent: video_surface_set_parent,
    surface_set_relative_position: video_surface_set_relative_position,
    surface_set_title: video_surface_set_title,
};

fn video_framebuffer_get_info() -> *mut FramebufferInfoC {
    video_bridge::framebuffer_get_info()
}

fn video_roulette_draw(fate: u32) -> core::ffi::c_int {
    match video_bridge::roulette_draw(fate) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn video_surface_enumerate_windows(out: *mut WindowInfo, max: u32) -> u32 {
    video_bridge::surface_enumerate_windows(out, max)
}

fn video_surface_set_window_position(id: u32, x: i32, y: i32) -> core::ffi::c_int {
    video_bridge::surface_set_window_position(id, x, y)
}

fn video_surface_set_window_state(id: u32, state: u8) -> core::ffi::c_int {
    video_bridge::surface_set_window_state(id, state)
}

fn video_surface_raise_window(id: u32) -> core::ffi::c_int {
    video_bridge::surface_raise_window(id)
}

fn video_surface_commit(id: u32) -> core::ffi::c_int {
    video_bridge::surface_commit(id)
}

fn video_register_surface(id: u32, w: u32, h: u32, token: u32) -> core::ffi::c_int {
    video_bridge::register_surface(id, w, h, token)
}

fn video_drain_queue() {
    video_bridge::drain_queue()
}

fn video_fb_flip(phys: PhysAddr, size: usize) -> core::ffi::c_int {
    video_bridge::fb_flip_from_shm(phys, size)
}

fn video_surface_request_frame_callback(id: u32) -> core::ffi::c_int {
    video_bridge::surface_request_frame_callback(id)
}

fn video_surface_mark_frames_done(time: u64) {
    video_bridge::surface_mark_frames_done(time)
}

fn video_surface_poll_frame_done(id: u32) -> u64 {
    video_bridge::surface_poll_frame_done(id)
}

fn video_surface_add_damage(id: u32, x: i32, y: i32, w: i32, h: i32) -> core::ffi::c_int {
    video_bridge::surface_add_damage(id, x, y, w, h)
}

fn video_surface_get_buffer_age(id: u32) -> u8 {
    video_bridge::surface_get_buffer_age(id)
}

fn video_surface_set_role(id: u32, role: u8) -> core::ffi::c_int {
    video_bridge::surface_set_role(id, role)
}

fn video_surface_set_parent(id: u32, parent: u32) -> core::ffi::c_int {
    video_bridge::surface_set_parent(id, parent)
}

fn video_surface_set_relative_position(id: u32, x: i32, y: i32) -> core::ffi::c_int {
    video_bridge::surface_set_relative_position(id, x, y)
}

fn video_surface_set_title(id: u32, ptr: *const u8, len: usize) -> core::ffi::c_int {
    let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
    video_bridge::surface_set_title(id, slice)
}

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
    register_video_services(&VIDEO_SERVICES);
    register_input_services(&INPUT_SERVICES);
    register_tty_services(&TTY_SERVICES);
    register_fate_services(&FATE_SERVICES);
}
