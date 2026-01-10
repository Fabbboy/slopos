#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use core::ffi::c_int;
use slopos_drivers::sched_bridge::register_video_task_cleanup_callback;
use slopos_drivers::serial_println;
use slopos_drivers::video_bridge;
use slopos_drivers::virtio_gpu;
use slopos_drivers::wl_currency;
use slopos_lib::FramebufferInfo;
use slopos_lib::klog_info;

use slopos_abi::WindowInfo;
use slopos_abi::addr::PhysAddr;
use slopos_abi::video_traits::{FramebufferInfoC, VideoServices};

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

// =============================================================================
// VideoServices trait implementation
// =============================================================================

/// Static instance of VideoServices for registration with drivers crate.
static VIDEO_SERVICES: VideoServicesImpl = VideoServicesImpl;

/// Implementation of VideoServices trait.
struct VideoServicesImpl;

impl VideoServices for VideoServicesImpl {
    fn framebuffer_get_info(&self) -> *mut FramebufferInfoC {
        framebuffer::framebuffer_get_info() as *mut FramebufferInfoC
    }

    fn roulette_draw(&self, fate: u32) -> c_int {
        match roulette_core::roulette_draw_kernel(fate) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    }

    fn surface_enumerate_windows(&self, out: *mut WindowInfo, max: u32) -> u32 {
        compositor_context::surface_enumerate_windows(out, max)
    }

    fn surface_set_window_position(&self, task_id: u32, x: i32, y: i32) -> c_int {
        compositor_context::surface_set_window_position(task_id, x, y)
            .map(|()| 0)
            .unwrap_or_else(|e| e.as_c_int())
    }

    fn surface_set_window_state(&self, task_id: u32, state: u8) -> c_int {
        compositor_context::surface_set_window_state(task_id, state)
            .map(|()| 0)
            .unwrap_or_else(|e| e.as_c_int())
    }

    fn surface_raise_window(&self, task_id: u32) -> c_int {
        compositor_context::surface_raise_window(task_id)
            .map(|()| 0)
            .unwrap_or_else(|e| e.as_c_int())
    }

    fn surface_commit(&self, task_id: u32) -> c_int {
        match compositor_context::surface_commit(task_id) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    }

    fn register_surface(&self, task_id: u32, width: u32, height: u32, shm_token: u32) -> c_int {
        compositor_context::register_surface_for_task(task_id, width, height, shm_token)
            .map(|()| 0)
            .unwrap_or_else(|e| e.as_c_int())
    }

    fn drain_queue(&self) {
        compositor_context::drain_queue();
    }

    fn fb_flip(&self, shm_phys: PhysAddr, size: usize) -> c_int {
        framebuffer::fb_flip_from_shm(shm_phys, size)
    }

    fn surface_request_frame_callback(&self, task_id: u32) -> c_int {
        compositor_context::surface_request_frame_callback(task_id)
            .map(|()| 0)
            .unwrap_or_else(|e| e.as_c_int())
    }

    fn surface_mark_frames_done(&self, present_time_ms: u64) {
        compositor_context::surface_mark_frames_done(present_time_ms);
    }

    fn surface_poll_frame_done(&self, task_id: u32) -> u64 {
        compositor_context::surface_poll_frame_done(task_id)
    }

    fn surface_add_damage(&self, task_id: u32, x: i32, y: i32, width: i32, height: i32) -> c_int {
        compositor_context::surface_add_damage(task_id, x, y, width, height)
            .map(|()| 0)
            .unwrap_or_else(|e| e.as_c_int())
    }

    fn surface_get_buffer_age(&self, task_id: u32) -> u8 {
        compositor_context::surface_get_buffer_age(task_id)
    }

    fn surface_set_role(&self, task_id: u32, role: u8) -> c_int {
        compositor_context::surface_set_role(task_id, role)
            .map(|()| 0)
            .unwrap_or_else(|e| e.as_c_int())
    }

    fn surface_set_parent(&self, task_id: u32, parent_task_id: u32) -> c_int {
        compositor_context::surface_set_parent(task_id, parent_task_id)
            .map(|()| 0)
            .unwrap_or_else(|e| e.as_c_int())
    }

    fn surface_set_relative_position(&self, task_id: u32, rel_x: i32, rel_y: i32) -> c_int {
        compositor_context::surface_set_relative_position(task_id, rel_x, rel_y)
            .map(|()| 0)
            .unwrap_or_else(|e| e.as_c_int())
    }

    fn surface_set_title(&self, task_id: u32, title_ptr: *const u8, title_len: usize) -> c_int {
        if title_ptr.is_null() {
            return -1;
        }

        // Validate pointer is in user address space
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
}

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

pub fn init(framebuffer: Option<FramebufferInfo>, backend: VideoBackend) {
    // Register task cleanup callback early so it's available even if framebuffer init fails
    register_video_task_cleanup_callback(task_cleanup_callback);

    let mut virgl_fb: Option<FramebufferInfo> = None;
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
            fb.width,
            fb.height,
            fb.pitch,
            fb.bpp
        );

        if framebuffer::init_with_info(fb) != 0 {
            serial_println!("Framebuffer init failed; skipping banner paint.");
            return;
        }

        // Register the trait object with the drivers crate
        video_bridge::register_video_services(&VIDEO_SERVICES);

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

fn try_init_virgl_backend() -> Option<FramebufferInfo> {
    let device = virtio_gpu::virtio_gpu_get_device();
    if device.is_null() {
        klog_info!("Video: virgl requested but no virtio-gpu device is present");
        wl_currency::award_loss();
        return None;
    }

    if !virtio_gpu::virtio_gpu_has_modern_caps() {
        klog_info!("Video: virgl requested; virtio-gpu lacks modern capabilities");
        wl_currency::award_loss();
        return None;
    }

    if !virtio_gpu::virtio_gpu_supports_virgl() {
        klog_info!("Video: virgl requested; device does not advertise virgl support");
        wl_currency::award_loss();
        return None;
    }

    if !virtio_gpu::virtio_gpu_is_virgl_ready() {
        klog_info!("Video: virgl requested; virtio-gpu context not ready");
        wl_currency::award_loss();
        return None;
    }

    match virtio_gpu::virtio_gpu_framebuffer_init() {
        Some(fb) => {
            klog_info!("Video: virgl requested; virtio-gpu framebuffer online");
            Some(fb)
        }
        None => {
            klog_info!("Video: virgl requested; virtio-gpu framebuffer init failed");
            wl_currency::award_loss();
            None
        }
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
