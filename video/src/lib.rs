#![no_std]
#![forbid(unsafe_code)]

use core::ffi::c_int;
use slopos_abi::FramebufferData;
use slopos_abi::addr::PhysAddr;
use slopos_abi::damage::DamageRect;
use slopos_abi::video_traits::VideoResult;
use slopos_core::task::register_task_resource_cleanup_hook;
#[cfg(feature = "xe-gpu")]
use slopos_drivers::xe;
use slopos_kernel_services::syscall_services::video::{
    VideoServices, compositor_task_id, register_video_services, set_compositor_task_id,
};
use slopos_ostd::{klog_info, klog_warn};

pub mod framebuffer;
pub mod graphics;
pub mod kernel_font;
pub mod panic_screen;
pub mod roulette_core;
pub mod splash;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoBackend {
    Framebuffer,
    #[cfg(feature = "xe-gpu")]
    Xe,
}

fn video_fb_flip(
    shm_phys: PhysAddr,
    size: usize,
    damage: *const DamageRect,
    damage_count: u32,
) -> c_int {
    framebuffer::fb_flip_from_shm_damage(shm_phys, size, damage, damage_count)
}

fn video_roulette_draw(fate: u32) -> VideoResult {
    roulette_core::roulette_draw_kernel(fate)
}

static VIDEO_SERVICES: VideoServices = VideoServices {
    get_display_info: framebuffer::get_display_info,
    fb_flip: video_fb_flip,
    roulette_draw: video_roulette_draw,
};

fn task_cleanup_callback(task_id: u32) {
    let cid = compositor_task_id();
    if cid != 0 && task_id == cid {
        set_compositor_task_id(0);
        framebuffer::release_compositor_fb();
    }
}

// =============================================================================
// Initialization
// =============================================================================

pub fn init(framebuffer: Option<FramebufferData>, _backend: VideoBackend) {
    register_task_resource_cleanup_hook(task_cleanup_callback);

    #[cfg(feature = "xe-gpu")]
    if _backend == VideoBackend::Xe {
        framebuffer::register_flush_callback(xe::xe_flush);
    }

    let fb_to_use = framebuffer;

    // Initialise the font subsystem (atlas + renderer) before any rendering.
    kernel_font::init();

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
    use slopos_abi::draw::Color32;
    use slopos_gfx::canvas_ops;

    let mut ctx = match graphics::GraphicsContext::new() {
        Ok(ctx) => ctx,
        Err(_) => return,
    };
    let banner_color = Color32(0x00AA_33AA);
    let w = ctx.width() as i32;
    let banner_h = (ctx.height() as i32).min(32);
    canvas_ops::fill_rect(&mut ctx, 0, 0, w, banner_h, banner_color);
}
