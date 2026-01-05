#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use core::ffi::c_int;
use slopos_drivers::serial_println;
use slopos_drivers::sched_bridge::register_video_task_cleanup_callback;
use slopos_drivers::video_bridge::{self, VideoCallbacks, VideoResult};
use slopos_lib::FramebufferInfo;

pub mod font;
pub mod framebuffer;
pub mod graphics;
pub mod panic_screen;
pub mod roulette_core;
pub mod compositor_context;
pub mod splash;

fn framebuffer_get_info_bridge() -> *mut slopos_drivers::video_bridge::FramebufferInfoC {
    framebuffer::framebuffer_get_info()
        as *mut slopos_drivers::video_bridge::FramebufferInfoC
}

fn roulette_draw_bridge(fate: u32) -> c_int {
    video_result_to_code(roulette_core::roulette_draw_kernel(fate))
}

fn surface_enumerate_windows_bridge(out_buffer: *mut video_bridge::WindowInfo, max_count: u32) -> u32 {
    // video_bridge::WindowInfo is a re-export of slopos_abi::WindowInfo, so this is now the same type
    compositor_context::surface_enumerate_windows(out_buffer, max_count)
}

fn surface_set_window_position_bridge(task_id: u32, x: i32, y: i32) -> c_int {
    compositor_context::surface_set_window_position(task_id, x, y)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn surface_set_window_state_bridge(task_id: u32, state: u8) -> c_int {
    compositor_context::surface_set_window_state(task_id, state)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn surface_raise_window_bridge(task_id: u32) -> c_int {
    compositor_context::surface_raise_window(task_id)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn surface_commit_bridge(task_id: u32) -> c_int {
    match compositor_context::surface_commit(task_id) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn register_surface_bridge(task_id: u32, width: u32, height: u32, shm_token: u32) -> c_int {
    compositor_context::register_surface_for_task(task_id, width, height, shm_token)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

/// Called when a task terminates to clean up its surface resources
fn task_cleanup_bridge(task_id: u32) {
    compositor_context::unregister_surface_for_task(task_id);
}

/// Drain the compositor queue - called by compositor at start of each frame
fn drain_queue_bridge() {
    compositor_context::drain_queue();
}

/// Copy from shared memory buffer to MMIO framebuffer (page flip for Wayland-like compositor)
fn fb_flip_bridge(shm_phys: u64, size: usize) -> c_int {
    framebuffer::fb_flip_from_shm(shm_phys, size)
}

/// Request a frame callback (Wayland wl_surface.frame)
fn surface_request_frame_callback_bridge(task_id: u32) -> c_int {
    compositor_context::surface_request_frame_callback(task_id)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

/// Mark frames as done (called by compositor after present)
fn surface_mark_frames_done_bridge(present_time_ms: u64) {
    compositor_context::surface_mark_frames_done(present_time_ms);
}

/// Poll for frame completion
fn surface_poll_frame_done_bridge(task_id: u32) -> u64 {
    compositor_context::surface_poll_frame_done(task_id)
}

/// Add damage region to surface
fn surface_add_damage_bridge(task_id: u32, x: i32, y: i32, width: i32, height: i32) -> c_int {
    compositor_context::surface_add_damage(task_id, x, y, width, height)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

/// Get back buffer age for damage accumulation
fn surface_get_buffer_age_bridge(task_id: u32) -> u8 {
    compositor_context::surface_get_buffer_age(task_id)
}

/// Set surface role (toplevel, popup, subsurface)
fn surface_set_role_bridge(task_id: u32, role: u8) -> c_int {
    compositor_context::surface_set_role(task_id, role)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

/// Set parent surface for subsurfaces
fn surface_set_parent_bridge(task_id: u32, parent_task_id: u32) -> c_int {
    compositor_context::surface_set_parent(task_id, parent_task_id)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

/// Set relative position for subsurfaces
fn surface_set_relative_position_bridge(task_id: u32, rel_x: i32, rel_y: i32) -> c_int {
    compositor_context::surface_set_relative_position(task_id, rel_x, rel_y)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

/// Set window title
fn surface_set_title_bridge(task_id: u32, title_ptr: *const u8, title_len: usize) -> c_int {
    if title_ptr.is_null() {
        return -1;
    }

    // Validate pointer is in user address space
    let ptr_addr = title_ptr as u64;
    let len = title_len.min(31);
    let end_addr = ptr_addr.saturating_add(len as u64);
    const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;
    if ptr_addr >= USER_SPACE_END || end_addr > USER_SPACE_END {
        return -1;
    }

    let title = unsafe { core::slice::from_raw_parts(title_ptr, len) };
    compositor_context::surface_set_title(task_id, title)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

pub fn init(framebuffer: Option<FramebufferInfo>) {
    // Register task cleanup callback early so it's available even if framebuffer init fails
    register_video_task_cleanup_callback(task_cleanup_bridge);

    if let Some(fb) = framebuffer {
        serial_println!(
            "Framebuffer online: {}x{} pitch {} bpp {}",
            fb.width,
            fb.height,
            fb.pitch,
            fb.bpp
        );

        if framebuffer::init_with_info(fb) != 0 {
            serial_println!("Framebuffer init failed; skipping banner paint.");
            return;
        }

        video_bridge::register_video_callbacks(VideoCallbacks {
            framebuffer_get_info: Some(framebuffer_get_info_bridge),
            roulette_draw: Some(roulette_draw_bridge),
            surface_enumerate_windows: Some(surface_enumerate_windows_bridge),
            surface_set_window_position: Some(surface_set_window_position_bridge),
            surface_set_window_state: Some(surface_set_window_state_bridge),
            surface_raise_window: Some(surface_raise_window_bridge),
            surface_commit: Some(surface_commit_bridge),
            fb_flip: Some(fb_flip_bridge),
            register_surface: Some(register_surface_bridge),
            drain_queue: Some(drain_queue_bridge),
            surface_request_frame_callback: Some(surface_request_frame_callback_bridge),
            surface_mark_frames_done: Some(surface_mark_frames_done_bridge),
            surface_poll_frame_done: Some(surface_poll_frame_done_bridge),
            surface_add_damage: Some(surface_add_damage_bridge),
            surface_get_buffer_age: Some(surface_get_buffer_age_bridge),
            surface_set_role: Some(surface_set_role_bridge),
            surface_set_parent: Some(surface_set_parent_bridge),
            surface_set_relative_position: Some(surface_set_relative_position_bridge),
            surface_set_title: Some(surface_set_title_bridge),
        });

        if let Err(err) = splash::splash_show_boot_screen() {
            serial_println!(
                "Splash paint failed ({:?}); falling back to banner stripe.",
                err
            );
            paint_banner();
        }
    } else {
        serial_println!("No framebuffer provided; skipping video init.");
    }
}

fn video_result_to_code(result: VideoResult) -> c_int {
    match result {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn paint_banner() {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return,
    };

    if fb.bpp < 24 {
        serial_println!(
            "Framebuffer bpp {} unsupported for banner paint; skipping.",
            fb.bpp
        );
        return;
    }

    // Paint a thin bar so the wizards see the Wheel spin in color.
    let stride = fb.pitch as usize;
    let height = fb.height.min(32) as usize;
    let width = fb.width as usize;
    let base = fb.base;

    for y in 0..height {
        for x in 0..width {
            let offset = y * stride + x * (fb.bpp as usize / 8);
            unsafe {
                let ptr = base.add(offset);
                // Simple purple slop hue: ARGB 0x00AA33AA
                ptr.write_volatile(0xAA);
                ptr.add(1).write_volatile(0x33);
                ptr.add(2).write_volatile(0xAA);
                ptr.add(3).write_volatile(0x00);
            }
        }
    }
}
