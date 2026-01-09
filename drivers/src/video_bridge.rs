//! Video bridge - thin wrapper over VideoServices trait object.
//!
//! This module provides the video subsystem interface for syscall handlers.
//! Uses trait objects to break the circular dependency between drivers and video crates.

use core::ffi::c_int;
use spin::Once;

// Re-export ABI types for consumers
pub use slopos_abi::video_traits::{FramebufferInfoC, VideoError, VideoResult, video_result_from_code};
pub use slopos_abi::{WindowDamageRect, WindowInfo, MAX_WINDOW_DAMAGE_REGIONS};

use slopos_abi::video_traits::VideoServices;

// =============================================================================
// Static trait object storage
// =============================================================================

static VIDEO: Once<&'static dyn VideoServices> = Once::new();

/// Register the video services implementation (called by boot crate).
pub fn register_video_services(svc: &'static dyn VideoServices) {
    VIDEO.call_once(|| svc);
}

// =============================================================================
// Macro for generating wrapper functions
// =============================================================================

macro_rules! video_fn {
    // Pattern for functions returning a value
    ($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty, $default:expr) => {
        pub fn $name($($arg: $ty),*) -> $ret {
            VIDEO.get().map(|v| v.$name($($arg),*)).unwrap_or($default)
        }
    };
    // Pattern for void functions
    ($name:ident($($arg:ident: $ty:ty),*)) => {
        pub fn $name($($arg: $ty),*) {
            if let Some(v) = VIDEO.get() { v.$name($($arg),*); }
        }
    };
}

// =============================================================================
// Generated wrapper functions
// =============================================================================

video_fn!(framebuffer_get_info() -> *mut FramebufferInfoC, core::ptr::null_mut());
video_fn!(surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32, 0);
video_fn!(surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int, -1);
video_fn!(surface_set_window_state(task_id: u32, state: u8) -> c_int, -1);
video_fn!(surface_raise_window(task_id: u32) -> c_int, -1);
video_fn!(surface_commit(task_id: u32) -> c_int, -1);
video_fn!(register_surface(task_id: u32, width: u32, height: u32, shm_token: u32) -> c_int, -1);
video_fn!(drain_queue());
video_fn!(surface_request_frame_callback(task_id: u32) -> c_int, -1);
video_fn!(surface_mark_frames_done(present_time_ms: u64));
video_fn!(surface_poll_frame_done(task_id: u32) -> u64, 0);
video_fn!(surface_add_damage(task_id: u32, x: i32, y: i32, width: i32, height: i32) -> c_int, -1);
video_fn!(surface_get_buffer_age(task_id: u32) -> u8, 0);
video_fn!(surface_set_role(task_id: u32, role: u8) -> c_int, -1);
video_fn!(surface_set_parent(task_id: u32, parent_task_id: u32) -> c_int, -1);
video_fn!(surface_set_relative_position(task_id: u32, rel_x: i32, rel_y: i32) -> c_int, -1);

// =============================================================================
// Special functions with custom logic
// =============================================================================

/// Draw roulette wheel - returns VideoResult instead of c_int.
pub fn roulette_draw(fate: u32) -> VideoResult {
    VIDEO.get()
        .map(|v| video_result_from_code(v.roulette_draw(fate)))
        .unwrap_or(Err(VideoError::NoFramebuffer))
}

/// Set window title - takes slice instead of raw pointer.
pub fn surface_set_title(task_id: u32, title: &[u8]) -> c_int {
    VIDEO.get()
        .map(|v| v.surface_set_title(task_id, title.as_ptr(), title.len()))
        .unwrap_or(-1)
}

/// Copy from a shared memory buffer to the MMIO framebuffer (page flip).
/// This is the "page flip" operation for the userland compositor.
pub fn fb_flip_from_shm(shm_phys: u64, size: usize) -> c_int {
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

    // Verify source address is valid via HHDM (use typed translation)
    use slopos_abi::addr::PhysAddr;
    use slopos_mm::hhdm::PhysAddrHhdm;
    if PhysAddr::new(shm_phys).to_virt_checked().is_none() {
        return -1;
    }

    VIDEO.get()
        .map(|v| v.fb_flip(shm_phys, copy_size))
        .unwrap_or(-1)
}
