//! Video services for syscall handlers
//!
//! These callbacks are registered by drivers and called by syscall handlers in core.

use core::ffi::c_int;
use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_abi::WindowInfo;
use slopos_abi::addr::PhysAddr;
use slopos_abi::video_traits::{FramebufferInfoC, VideoResult, video_result_from_code};

/// Video service callbacks - registered by drivers, called by syscall handlers
#[repr(C)]
pub struct VideoServices {
    pub framebuffer_get_info: fn() -> *mut FramebufferInfoC,
    pub roulette_draw: fn(u32) -> c_int,
    pub surface_enumerate_windows: fn(*mut WindowInfo, u32) -> u32,
    pub surface_set_window_position: fn(u32, i32, i32) -> c_int,
    pub surface_set_window_state: fn(u32, u8) -> c_int,
    pub surface_raise_window: fn(u32) -> c_int,
    pub surface_commit: fn(u32) -> c_int,
    pub register_surface: fn(u32, u32, u32, u32) -> c_int,
    pub drain_queue: fn(),
    pub fb_flip: fn(PhysAddr, usize) -> c_int,
    pub surface_request_frame_callback: fn(u32) -> c_int,
    pub surface_mark_frames_done: fn(u64),
    pub surface_poll_frame_done: fn(u32) -> u64,
    pub surface_add_damage: fn(u32, i32, i32, i32, i32) -> c_int,
    pub surface_get_buffer_age: fn(u32) -> u8,
    pub surface_set_role: fn(u32, u8) -> c_int,
    pub surface_set_parent: fn(u32, u32) -> c_int,
    pub surface_set_relative_position: fn(u32, i32, i32) -> c_int,
    pub surface_set_title: fn(u32, *const u8, usize) -> c_int,
}

static VIDEO: AtomicPtr<VideoServices> = AtomicPtr::new(core::ptr::null_mut());

/// Register video services - called once by drivers during init
pub fn register_video_services(services: &'static VideoServices) {
    let prev = VIDEO.swap(services as *const _ as *mut _, Ordering::Release);
    assert!(prev.is_null(), "video services already registered");
}

/// Check if video services are registered
pub fn is_video_initialized() -> bool {
    !VIDEO.load(Ordering::Acquire).is_null()
}

/// Get video services - panics if not initialized (kernel invariant)
#[inline(always)]
pub fn video_services() -> &'static VideoServices {
    let ptr = VIDEO.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "video services not initialized");
    unsafe { &*ptr }
}

// =============================================================================
// Convenience wrappers (match existing video_bridge API)
// =============================================================================

#[inline(always)]
pub fn framebuffer_get_info() -> *mut FramebufferInfoC {
    (video_services().framebuffer_get_info)()
}

#[inline(always)]
pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    (video_services().surface_enumerate_windows)(out_buffer, max_count)
}

#[inline(always)]
pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    (video_services().surface_set_window_position)(task_id, x, y)
}

#[inline(always)]
pub fn surface_set_window_state(task_id: u32, state: u8) -> c_int {
    (video_services().surface_set_window_state)(task_id, state)
}

#[inline(always)]
pub fn surface_raise_window(task_id: u32) -> c_int {
    (video_services().surface_raise_window)(task_id)
}

#[inline(always)]
pub fn surface_commit(task_id: u32) -> c_int {
    (video_services().surface_commit)(task_id)
}

#[inline(always)]
pub fn register_surface(task_id: u32, width: u32, height: u32, shm_token: u32) -> c_int {
    (video_services().register_surface)(task_id, width, height, shm_token)
}

#[inline(always)]
pub fn drain_queue() {
    (video_services().drain_queue)()
}

#[inline(always)]
pub fn fb_flip_from_shm(phys_addr: PhysAddr, size: usize) -> c_int {
    (video_services().fb_flip)(phys_addr, size)
}

#[inline(always)]
pub fn surface_request_frame_callback(task_id: u32) -> c_int {
    (video_services().surface_request_frame_callback)(task_id)
}

#[inline(always)]
pub fn surface_mark_frames_done(present_time_ms: u64) {
    (video_services().surface_mark_frames_done)(present_time_ms)
}

#[inline(always)]
pub fn surface_poll_frame_done(task_id: u32) -> u64 {
    (video_services().surface_poll_frame_done)(task_id)
}

#[inline(always)]
pub fn surface_add_damage(task_id: u32, x: i32, y: i32, width: i32, height: i32) -> c_int {
    (video_services().surface_add_damage)(task_id, x, y, width, height)
}

#[inline(always)]
pub fn surface_get_buffer_age(task_id: u32) -> u8 {
    (video_services().surface_get_buffer_age)(task_id)
}

#[inline(always)]
pub fn surface_set_role(task_id: u32, role: u8) -> c_int {
    (video_services().surface_set_role)(task_id, role)
}

#[inline(always)]
pub fn surface_set_parent(task_id: u32, parent_task_id: u32) -> c_int {
    (video_services().surface_set_parent)(task_id, parent_task_id)
}

#[inline(always)]
pub fn surface_set_relative_position(task_id: u32, rel_x: i32, rel_y: i32) -> c_int {
    (video_services().surface_set_relative_position)(task_id, rel_x, rel_y)
}

#[inline(always)]
pub fn roulette_draw(fate: u32) -> VideoResult {
    video_result_from_code((video_services().roulette_draw)(fate))
}

#[inline(always)]
pub fn surface_set_title(task_id: u32, title: &[u8]) -> c_int {
    (video_services().surface_set_title)(task_id, title.as_ptr(), title.len())
}
