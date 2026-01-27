#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use core::ffi::c_int;
use slopos_abi::addr::PhysAddr;
use slopos_abi::video_traits::VideoResult;
use slopos_abi::CompositorError;
use slopos_abi::FramebufferData;
use slopos_core::syscall_services::{register_video_services, VideoServices};
use slopos_core::task::register_video_cleanup_hook;
use slopos_drivers::xe;
use slopos_lib::{klog_info, klog_warn};

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
    Xe,
}

fn video_fb_flip(shm_phys: PhysAddr, size: usize) -> c_int {
    framebuffer::fb_flip_from_shm(shm_phys, size)
}

fn video_roulette_draw(fate: u32) -> VideoResult {
    roulette_core::roulette_draw_kernel(fate)
}

fn video_surface_set_title(
    task_id: u32,
    title_ptr: *const u8,
    title_len: usize,
) -> Result<(), CompositorError> {
    if title_ptr.is_null() {
        return Err(CompositorError::InvalidArgument);
    }

    let ptr_addr = title_ptr as u64;
    let len = title_len.min(31);
    let end_addr = ptr_addr.saturating_add(len as u64);
    use slopos_mm::mm_constants::USER_SPACE_END_VA;
    if ptr_addr >= USER_SPACE_END_VA || end_addr > USER_SPACE_END_VA {
        return Err(CompositorError::InvalidArgument);
    }

    let title = unsafe { core::slice::from_raw_parts(title_ptr, len) };
    compositor_context::surface_set_title(task_id, title)
}

static VIDEO_SERVICES: VideoServices = VideoServices {
    get_display_info: framebuffer::get_display_info,
    roulette_draw: video_roulette_draw,
    surface_enumerate_windows: compositor_context::surface_enumerate_windows,
    surface_set_window_position: compositor_context::surface_set_window_position,
    surface_set_window_state: compositor_context::surface_set_window_state,
    surface_raise_window: compositor_context::surface_raise_window,
    surface_commit: compositor_context::surface_commit,
    register_surface: compositor_context::register_surface_for_task,
    drain_queue: compositor_context::drain_queue,
    fb_flip: video_fb_flip,
    surface_request_frame_callback: compositor_context::surface_request_frame_callback,
    surface_mark_frames_done: compositor_context::surface_mark_frames_done,
    surface_poll_frame_done: compositor_context::surface_poll_frame_done,
    surface_add_damage: compositor_context::surface_add_damage,
    surface_get_buffer_age: compositor_context::surface_get_buffer_age,
    surface_set_role: compositor_context::surface_set_role,
    surface_set_parent: compositor_context::surface_set_parent,
    surface_set_relative_position: compositor_context::surface_set_relative_position,
    surface_set_title: video_surface_set_title,
};

fn task_cleanup_callback(task_id: u32) {
    compositor_context::unregister_surface_for_task(task_id);
}

// =============================================================================
// Initialization
// =============================================================================

pub fn init(framebuffer: Option<FramebufferData>, backend: VideoBackend) {
    register_video_cleanup_hook(task_cleanup_callback);

    if backend == VideoBackend::Xe {
        framebuffer::register_flush_callback(xe::xe_flush);
    }

    let fb_to_use = framebuffer;

    if let Some(fb) = fb_to_use {
        klog_info!(
            "Framebuffer online: {}x{} pitch {} bpp {}",
            fb.info.width,
            fb.info.height,
            fb.info.pitch,
            fb.info.bytes_per_pixel() * 8
        );

        if framebuffer::init_with_display_info(fb.address, &fb.info) != 0 {
            klog_warn!("Framebuffer init failed; skipping banner paint.");
            return;
        }

        register_video_services(&VIDEO_SERVICES);

        if let Err(err) = splash::splash_show_boot_screen() {
            klog_warn!(
                "Splash paint failed ({:?}); falling back to banner stripe.",
                err
            );
            paint_banner();
        }
        framebuffer::framebuffer_flush();
    } else {
        klog_warn!("No framebuffer provided; skipping video init.");
    }
}

fn paint_banner() {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return,
    };

    if fb.bpp() < 24 {
        klog_warn!(
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
