use core::ffi::c_int;

use slopos_abi::CompositorError;
use slopos_abi::DisplayInfo;
use slopos_abi::WindowInfo;
use slopos_abi::addr::PhysAddr;
use slopos_abi::video_traits::VideoResult;

pub type CompositorResult = Result<(), CompositorError>;

slopos_lib::define_service! {
    video => VideoServices {
        get_display_info() -> Option<DisplayInfo>;
        surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32;
        surface_set_window_position(task_id: u32, x: i32, y: i32) -> CompositorResult;
        surface_set_window_state(task_id: u32, state: u8) -> CompositorResult;
        surface_raise_window(task_id: u32) -> CompositorResult;
        surface_commit(task_id: u32) -> CompositorResult;
        register_surface(task_id: u32, width: u32, height: u32, shm_token: u32) -> CompositorResult;
        drain_queue();
        surface_request_frame_callback(task_id: u32) -> CompositorResult;
        surface_mark_frames_done(present_time_ms: u64);
        surface_poll_frame_done(task_id: u32) -> u64;
        surface_add_damage(task_id: u32, x: i32, y: i32, width: i32, height: i32) -> CompositorResult;
        surface_get_buffer_age(task_id: u32) -> u8;
        surface_set_role(task_id: u32, role: u8) -> CompositorResult;
        surface_set_parent(task_id: u32, parent_task_id: u32) -> CompositorResult;
        surface_set_relative_position(task_id: u32, rel_x: i32, rel_y: i32) -> CompositorResult;
        @no_wrapper fb_flip(phys_addr: PhysAddr, size: usize) -> c_int;
        @no_wrapper roulette_draw(fate: u32) -> VideoResult;
        @no_wrapper surface_set_title(task_id: u32, ptr: *const u8, len: usize) -> CompositorResult;
    }
}

#[inline(always)]
pub fn fb_flip_from_shm(phys_addr: PhysAddr, size: usize) -> c_int {
    (video_services().fb_flip)(phys_addr, size)
}

#[inline(always)]
pub fn roulette_draw(fate: u32) -> VideoResult {
    (video_services().roulette_draw)(fate)
}

#[inline(always)]
pub fn surface_set_title(task_id: u32, title: &[u8]) -> CompositorResult {
    (video_services().surface_set_title)(task_id, title.as_ptr(), title.len())
}
