#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use core::ffi::c_int;
use slopos_drivers::serial_println;
use slopos_drivers::scheduler_callbacks::register_video_task_cleanup_callback;
use slopos_drivers::video_bridge::{self, VideoCallbacks, VideoResult};
use slopos_lib::FramebufferInfo;

pub mod compositor;
pub mod font;
pub mod framebuffer;
pub mod graphics;
pub mod roulette_core;
pub mod splash;

// Re-export compositor types for backward compatibility
pub use compositor::events::{WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL};
pub use compositor::{WindowDamageRect, WindowInfo, MAX_WINDOW_DAMAGE_REGIONS};

fn framebuffer_get_info_bridge() -> *mut slopos_drivers::video_bridge::FramebufferInfoC {
    framebuffer::framebuffer_get_info()
        as *mut slopos_drivers::video_bridge::FramebufferInfoC
}

fn roulette_draw_bridge(fate: u32) -> c_int {
    video_result_to_code(roulette_core::roulette_draw_kernel(fate))
}

fn surface_enumerate_windows_bridge(out_buffer: *mut video_bridge::WindowInfo, max_count: u32) -> u32 {
    compositor::api::surface_enumerate_windows(out_buffer as *mut compositor::WindowInfo, max_count)
}

fn surface_set_window_position_bridge(task_id: u32, x: i32, y: i32) -> c_int {
    compositor::api::surface_set_window_position(task_id, x, y)
}

fn surface_set_window_state_bridge(task_id: u32, state: u8) -> c_int {
    compositor::api::surface_set_window_state(task_id, state)
}

fn surface_raise_window_bridge(task_id: u32) -> c_int {
    compositor::api::surface_raise_window(task_id)
}

fn surface_commit_bridge(task_id: u32) -> c_int {
    match compositor::api::surface_commit(task_id) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn register_surface_bridge(task_id: u32, width: u32, height: u32, bpp: u8) -> c_int {
    compositor::api::register_surface_for_task(task_id, width, height, bpp)
}

/// Called when a task terminates to clean up its surface resources
fn task_cleanup_bridge(task_id: u32) {
    compositor::api::unregister_surface_for_task(task_id);
}

/// Copy from shared memory buffer to MMIO framebuffer (page flip for Wayland-like compositor)
fn fb_flip_bridge(shm_phys: u64, size: usize) -> c_int {
    compositor::api::fb_flip_from_shm(shm_phys, size)
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

        // Initialize the legacy framebuffer module (for splash/roulette)
        if framebuffer::init_with_info(fb) != 0 {
            serial_println!("Framebuffer init failed; skipping banner paint.");
            return;
        }

        // Initialize the new event-driven compositor
        if compositor::api::init(fb) != 0 {
            serial_println!("Compositor init failed");
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
    let bytes_per_pixel = (fb.bpp as usize) / 8;
    let base = fb.base;

    // Calculate total framebuffer size for bounds checking
    let fb_size = stride.saturating_mul(fb.height as usize);
    if fb_size == 0 {
        return;
    }

    for y in 0..height {
        for x in 0..width {
            let offset = y * stride + x * bytes_per_pixel;
            // Bounds check: ensure we can write 4 bytes at this offset
            if offset.saturating_add(4) > fb_size {
                continue;
            }
            // SAFETY: We verified offset + 4 <= fb_size, and base points to
            // valid MMIO framebuffer memory of at least fb_size bytes.
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
