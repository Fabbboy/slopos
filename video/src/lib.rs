#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use core::ffi::c_int;
use slopos_abi::FramebufferData;
use slopos_abi::WindowInfo;
use slopos_abi::addr::PhysAddr;
use slopos_abi::video_traits::FramebufferInfoC;
use slopos_core::syscall_services::{VideoServices, register_video_services};
use slopos_core::task::register_video_cleanup_hook;
use slopos_drivers::serial_println;
use slopos_drivers::virtio_gpu;
use slopos_lib::klog_info;

pub mod compositor_context;
pub mod font;
pub mod framebuffer;
pub mod graphics;
pub mod panic_screen;
pub mod roulette_core;
pub mod splash;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoBackend {
    Framebuffer,
    Virgl,
}

fn video_framebuffer_get_info() -> *mut FramebufferInfoC {
    framebuffer::framebuffer_get_info() as *mut FramebufferInfoC
}

fn video_roulette_draw(fate: u32) -> c_int {
    match roulette_core::roulette_draw_kernel(fate) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn video_surface_enumerate_windows(out: *mut WindowInfo, max: u32) -> u32 {
    compositor_context::surface_enumerate_windows(out, max)
}

fn video_surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    compositor_context::surface_set_window_position(task_id, x, y)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn video_surface_set_window_state(task_id: u32, state: u8) -> c_int {
    compositor_context::surface_set_window_state(task_id, state)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn video_surface_raise_window(task_id: u32) -> c_int {
    compositor_context::surface_raise_window(task_id)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn video_surface_commit(task_id: u32) -> c_int {
    match compositor_context::surface_commit(task_id) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn video_register_surface(task_id: u32, width: u32, height: u32, shm_token: u32) -> c_int {
    compositor_context::register_surface_for_task(task_id, width, height, shm_token)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn video_drain_queue() {
    compositor_context::drain_queue();
}

fn video_fb_flip(shm_phys: PhysAddr, size: usize) -> c_int {
    framebuffer::fb_flip_from_shm(shm_phys, size)
}

fn video_surface_request_frame_callback(task_id: u32) -> c_int {
    compositor_context::surface_request_frame_callback(task_id)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn video_surface_mark_frames_done(present_time_ms: u64) {
    compositor_context::surface_mark_frames_done(present_time_ms);
}

fn video_surface_poll_frame_done(task_id: u32) -> u64 {
    compositor_context::surface_poll_frame_done(task_id)
}

fn video_surface_add_damage(task_id: u32, x: i32, y: i32, width: i32, height: i32) -> c_int {
    compositor_context::surface_add_damage(task_id, x, y, width, height)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn video_surface_get_buffer_age(task_id: u32) -> u8 {
    compositor_context::surface_get_buffer_age(task_id)
}

fn video_surface_set_role(task_id: u32, role: u8) -> c_int {
    compositor_context::surface_set_role(task_id, role)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn video_surface_set_parent(task_id: u32, parent_task_id: u32) -> c_int {
    compositor_context::surface_set_parent(task_id, parent_task_id)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn video_surface_set_relative_position(task_id: u32, rel_x: i32, rel_y: i32) -> c_int {
    compositor_context::surface_set_relative_position(task_id, rel_x, rel_y)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

fn video_surface_set_title(task_id: u32, title_ptr: *const u8, title_len: usize) -> c_int {
    if title_ptr.is_null() {
        return -1;
    }

    let ptr_addr = title_ptr as u64;
    let len = title_len.min(31);
    let end_addr = ptr_addr.saturating_add(len as u64);
    use slopos_mm::mm_constants::USER_SPACE_END_VA;
    if ptr_addr >= USER_SPACE_END_VA || end_addr > USER_SPACE_END_VA {
        return -1;
    }

    let title = unsafe { core::slice::from_raw_parts(title_ptr, len) };
    compositor_context::surface_set_title(task_id, title)
        .map(|()| 0)
        .unwrap_or_else(|e| e.as_c_int())
}

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

// =============================================================================
// Task cleanup callback
// =============================================================================

/// Called when a task terminates to clean up its surface resources.
fn task_cleanup_callback(task_id: u32) {
    compositor_context::unregister_surface_for_task(task_id);
}

// =============================================================================
// Initialization
// =============================================================================

pub fn init(framebuffer: Option<FramebufferData>, backend: VideoBackend) {
    register_video_cleanup_hook(task_cleanup_callback);

    let mut virgl_fb: Option<FramebufferData> = None;
    if backend == VideoBackend::Virgl {
        virgl_fb = try_init_virgl_backend();
        if virgl_fb.is_none() {
            klog_info!("Video: virgl backend unavailable; falling back to framebuffer");
        } else {
            framebuffer::register_flush_callback(virtio_gpu::virtio_gpu_flush_full);
        }
    }

    let fb_to_use = if virgl_fb.is_some() {
        virgl_fb
    } else {
        framebuffer
    };

    if let Some(fb) = fb_to_use {
        serial_println!(
            "Framebuffer online: {}x{} pitch {} bpp {}",
            fb.info.width,
            fb.info.height,
            fb.info.pitch,
            fb.info.bytes_per_pixel() * 8
        );

        if framebuffer::init_with_display_info(fb.address, &fb.info) != 0 {
            serial_println!("Framebuffer init failed; skipping banner paint.");
            return;
        }

        register_video_services(&VIDEO_SERVICES);

        if let Err(err) = splash::splash_show_boot_screen() {
            serial_println!(
                "Splash paint failed ({:?}); falling back to banner stripe.",
                err
            );
            paint_banner();
        }
        framebuffer::framebuffer_flush();
    } else {
        serial_println!("No framebuffer provided; skipping video init.");
    }
}

fn try_init_virgl_backend() -> Option<FramebufferData> {
    let device = virtio_gpu::virtio_gpu_get_device();
    if device.is_null() {
        klog_info!("Video: virgl requested but no virtio-gpu device is present");
        return None;
    }

    if !virtio_gpu::virtio_gpu_has_modern_caps() {
        klog_info!("Video: virgl requested; virtio-gpu lacks modern capabilities");
        return None;
    }

    if !virtio_gpu::virtio_gpu_supports_virgl() {
        klog_info!("Video: virgl requested; device does not advertise virgl support");
        return None;
    }

    if !virtio_gpu::virtio_gpu_is_virgl_ready() {
        klog_info!("Video: virgl requested; virtio-gpu context not ready");
        return None;
    }

    match virtio_gpu::virtio_gpu_framebuffer_init() {
        Some(fb) => {
            klog_info!("Video: virgl requested; virtio-gpu framebuffer online");
            Some(fb)
        }
        None => {
            klog_info!("Video: virgl requested; virtio-gpu framebuffer init failed");
            None
        }
    }
}

fn paint_banner() {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return,
    };

    if fb.bpp() < 24 {
        serial_println!(
            "Framebuffer bpp {} unsupported for banner paint; skipping.",
            fb.bpp()
        );
        return;
    }

    let stride = fb.pitch() as usize;
    let height = fb.height().min(32) as usize;
    let width = fb.width() as usize;
    let base = fb.base_ptr();

    for y in 0..height {
        for x in 0..width {
            let offset = y * stride + x * (fb.bpp() as usize / 8);
            unsafe {
                let ptr = base.add(offset);
                ptr.write_volatile(0xAA);
                ptr.add(1).write_volatile(0x33);
                ptr.add(2).write_volatile(0xAA);
                ptr.add(3).write_volatile(0x00);
            }
        }
    }
}
