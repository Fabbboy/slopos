use core::ffi::c_int;
use core::sync::atomic::{AtomicPtr, Ordering};

pub use slopos_abi::video_traits::{
    FramebufferInfoC, VideoError, VideoResult, video_result_from_code,
};
pub use slopos_abi::{MAX_WINDOW_DAMAGE_REGIONS, WindowDamageRect, WindowInfo};

use slopos_abi::addr::PhysAddr;

#[repr(C)]
pub struct VideoCallbacks {
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

static VIDEO: AtomicPtr<VideoCallbacks> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_video_services(callbacks: &'static VideoCallbacks) {
    VIDEO.store(callbacks as *const _ as *mut _, Ordering::Release);
}

#[inline(always)]
fn video() -> Option<&'static VideoCallbacks> {
    let ptr = VIDEO.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}

pub fn framebuffer_get_info() -> *mut FramebufferInfoC {
    video()
        .map(|v| (v.framebuffer_get_info)())
        .unwrap_or(core::ptr::null_mut())
}

pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    video()
        .map(|v| (v.surface_enumerate_windows)(out_buffer, max_count))
        .unwrap_or(0)
}

pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    video()
        .map(|v| (v.surface_set_window_position)(task_id, x, y))
        .unwrap_or(-1)
}

pub fn surface_set_window_state(task_id: u32, state: u8) -> c_int {
    video()
        .map(|v| (v.surface_set_window_state)(task_id, state))
        .unwrap_or(-1)
}

pub fn surface_raise_window(task_id: u32) -> c_int {
    video()
        .map(|v| (v.surface_raise_window)(task_id))
        .unwrap_or(-1)
}

pub fn surface_commit(task_id: u32) -> c_int {
    video().map(|v| (v.surface_commit)(task_id)).unwrap_or(-1)
}

pub fn register_surface(task_id: u32, width: u32, height: u32, shm_token: u32) -> c_int {
    video()
        .map(|v| (v.register_surface)(task_id, width, height, shm_token))
        .unwrap_or(-1)
}

pub fn drain_queue() {
    if let Some(v) = video() {
        (v.drain_queue)();
    }
}

pub fn surface_request_frame_callback(task_id: u32) -> c_int {
    video()
        .map(|v| (v.surface_request_frame_callback)(task_id))
        .unwrap_or(-1)
}

pub fn surface_mark_frames_done(present_time_ms: u64) {
    if let Some(v) = video() {
        (v.surface_mark_frames_done)(present_time_ms);
    }
}

pub fn surface_poll_frame_done(task_id: u32) -> u64 {
    video()
        .map(|v| (v.surface_poll_frame_done)(task_id))
        .unwrap_or(0)
}

pub fn surface_add_damage(task_id: u32, x: i32, y: i32, width: i32, height: i32) -> c_int {
    video()
        .map(|v| (v.surface_add_damage)(task_id, x, y, width, height))
        .unwrap_or(-1)
}

pub fn surface_get_buffer_age(task_id: u32) -> u8 {
    video()
        .map(|v| (v.surface_get_buffer_age)(task_id))
        .unwrap_or(0)
}

pub fn surface_set_role(task_id: u32, role: u8) -> c_int {
    video()
        .map(|v| (v.surface_set_role)(task_id, role))
        .unwrap_or(-1)
}

pub fn surface_set_parent(task_id: u32, parent_task_id: u32) -> c_int {
    video()
        .map(|v| (v.surface_set_parent)(task_id, parent_task_id))
        .unwrap_or(-1)
}

pub fn surface_set_relative_position(task_id: u32, rel_x: i32, rel_y: i32) -> c_int {
    video()
        .map(|v| (v.surface_set_relative_position)(task_id, rel_x, rel_y))
        .unwrap_or(-1)
}

pub fn roulette_draw(fate: u32) -> VideoResult {
    video()
        .map(|v| video_result_from_code((v.roulette_draw)(fate)))
        .unwrap_or(Err(VideoError::NoFramebuffer))
}

pub fn surface_set_title(task_id: u32, title: &[u8]) -> c_int {
    video()
        .map(|v| (v.surface_set_title)(task_id, title.as_ptr(), title.len()))
        .unwrap_or(-1)
}

pub fn fb_flip_from_shm(shm_phys: PhysAddr, size: usize) -> c_int {
    let fb_ptr = framebuffer_get_info();
    if fb_ptr.is_null() {
        return -1;
    }

    let fb_info = unsafe { &*fb_ptr };
    if fb_info.initialized == 0 {
        return -1;
    }

    let fb_size = (fb_info.pitch * fb_info.height) as usize;
    let copy_size = size.min(fb_size);
    if copy_size == 0 {
        return -1;
    }

    use slopos_mm::hhdm::PhysAddrHhdm;
    if shm_phys.to_virt_checked().is_none() {
        return -1;
    }

    video()
        .map(|v| (v.fb_flip)(shm_phys, copy_size))
        .unwrap_or(-1)
}
