#![no_std]
#![forbid(unsafe_code)]

use core::ffi::c_int;
use slopos_abi::FramebufferData;
use slopos_abi::addr::PhysAddr;
use slopos_abi::damage::DamageRect;
use slopos_abi::video_traits::VideoResult;
use slopos_kernel_services::syscall_services::scanout::{
    self, InstallCtx, ScanoutId, ScanoutProvider,
};
use slopos_kernel_services::syscall_services::video::{
    VideoServices, compositor_task_id, is_video_initialized, register_video_services,
    set_compositor_task_id,
};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{klog_info, klog_warn};
use slopos_sched::task::register_task_resource_cleanup_hook;

pub mod fblog;
pub mod framebuffer;
pub mod graphics;
pub mod kernel_font;
pub mod panic_screen;
pub mod roulette_core;
pub mod splash;

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
    hw_cursor_available: video_hw_cursor_available,
    cursor_set_image: video_cursor_set_image,
    cursor_move: video_cursor_move,
    set_display_mode: video_set_display_mode,
};

// =============================================================================
// GPU control backend (hardware cursor + runtime mode-set)
//
// Boot registers the driver's functions here behind a fn-pointer indirection,
// so the video crate needs no dependency on `slopos-drivers`.
// =============================================================================

#[derive(Clone, Copy)]
struct GpuControl {
    available: fn() -> bool,
    set_image: fn(*const u8, usize, u32, u32) -> bool,
    move_cursor: fn(u32, u32) -> bool,
    set_mode: fn(u32, u32) -> Option<FramebufferData>,
}

static GPU_CONTROL: SpinLock<Option<GpuControl>> = SpinLock::new(None, LOCK_LEVEL_RESOURCE);

/// Register the GPU control backend (called by boot once virtio-gpu is adopted).
pub fn register_gpu_control(
    available: fn() -> bool,
    set_image: fn(*const u8, usize, u32, u32) -> bool,
    move_cursor: fn(u32, u32) -> bool,
    set_mode: fn(u32, u32) -> Option<FramebufferData>,
) {
    *GPU_CONTROL.lock() = Some(GpuControl {
        available,
        set_image,
        move_cursor,
        set_mode,
    });
}

/// Copy the backend out and drop the lock before any (blocking) GPU call — the
/// SpinLock disables IRQs, and the driver blocks on GPU command completion.
fn gpu_control() -> Option<GpuControl> {
    *GPU_CONTROL.lock()
}

fn video_hw_cursor_available() -> bool {
    gpu_control().map(|g| (g.available)()).unwrap_or(false)
}

fn video_cursor_set_image(image: *const u8, len: usize, hot_x: u32, hot_y: u32) -> bool {
    match gpu_control() {
        Some(g) => (g.set_image)(image, len, hot_x, hot_y),
        None => false,
    }
}

fn video_cursor_move(x: u32, y: u32) -> bool {
    match gpu_control() {
        Some(g) => (g.move_cursor)(x, y),
        None => false,
    }
}

fn video_set_display_mode(width: u32, height: u32) -> bool {
    let Some(g) = gpu_control() else {
        return false;
    };
    match (g.set_mode)(width, height) {
        Some(fb) => framebuffer::init_with_display_info(fb.address, &fb.info) == 0,
        None => false,
    }
}

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

/// Bring up the video subsystem and register the firmware framebuffer as the
/// default scanout provider.
///
/// `firmware_priority` is the arbitration priority the firmware backing claims
/// at: normally [`scanout::PRIO_FIRMWARE_FB`], but boot lifts it above every GPU
/// (via [`scanout::PRIO_CMDLINE_HINT_BUMP`]) when the cmdline forces the passive
/// framebuffer, so later GPU probes lose arbitration and never reset the device.
pub fn init(framebuffer: Option<FramebufferData>, firmware_priority: i32) {
    register_task_resource_cleanup_hook(task_cleanup_callback);

    // Initialise the font subsystem (atlas + renderer) before any rendering.
    kernel_font::init();

    // Register the on-screen kernel-log (fblog) renderer with the ostd core.
    // Inert until the `fblog=` cmdline knob (or ESC) activates it.
    fblog::init();

    // Expose the scanout install logic to the arbiter so GPU drivers can adopt a
    // scanout without naming a `video` symbol.
    scanout::register_scanout_installer(install_scanout_provider);

    if let Some(fb) = framebuffer {
        klog_info!(
            "Framebuffer online: {}x{} pitch {} bpp {}",
            fb.info.width,
            fb.info.height,
            fb.info.pitch,
            fb.info.bytes_per_pixel() * 8
        );

        // Claim and commit the firmware framebuffer as the default scanout owner
        // before any GPU is probed, then install it.
        scanout::SCANOUT.claim(firmware_priority);
        scanout::SCANOUT.commit_install(
            ScanoutProvider {
                id: ScanoutId::FirmwareFb,
                priority: firmware_priority,
                evict: firmware_evict,
            },
            firmware_priority,
            |_| {},
        );
        if !install_scanout_provider(&InstallCtx {
            fb,
            flush: None,
            gpu_control: None,
        }) {
            klog_warn!("Framebuffer init failed; skipping banner paint.");
        }
    } else {
        klog_warn!("No framebuffer provided; skipping video init.");
    }
}

/// Paint the boot splash on the current framebuffer and present it.
///
/// Driven by framebuffer availability, not a fixed boot step: called both when
/// the framebuffer first comes up and after a backend upgrade (virtio-gpu
/// adoption) so the boot console reappears on the new scanout. No-op while the
/// on-screen log owns the screen — it repaints itself on its timer tick.
pub fn show_splash() {
    if slopos_ostd::fblog::is_active() {
        return;
    }
    if let Err(err) = splash::splash_show_boot_screen() {
        klog_warn!(
            "Splash paint failed ({:?}); falling back to banner stripe.",
            err
        );
        paint_banner();
    }
    framebuffer::framebuffer_flush(core::ptr::null(), 0);
}

/// Eviction hook for the firmware framebuffer: nothing to tear down (it is a
/// passive direct-write backing).
fn firmware_evict() {}

/// Adopt a scanout backing and wire the front-end to it. The single install path
/// for every provider: the firmware framebuffer (`flush`/`gpu_control` both
/// `None`, a passive direct-write backing) and GPU scanouts alike.
///
/// Registered with the arbiter via [`scanout::register_scanout_installer`] so GPU
/// drivers in `slopos-drivers` can drive it through a fn-pointer. Re-points the
/// framebuffer state at `ctx.fb.address` (so the compositor's `fb_flip` copies
/// land there), routes presents through `ctx.flush` when present, re-registers
/// the vconsole on the new backing, exposes the hardware cursor / mode-set, syncs
/// the mouse bounds, and repaints the boot console. Returns `true` on success.
fn install_scanout_provider(ctx: &InstallCtx) -> bool {
    if framebuffer::init_with_display_info(ctx.fb.address, &ctx.fb.info) != 0 {
        klog_warn!("scanout: framebuffer adoption failed; staying on previous backend");
        return false;
    }

    if let Some(flush) = ctx.flush {
        framebuffer::register_flush_callback(flush);
    }

    // The early firmware install already registered the table; register here only
    // if it was skipped (no early framebuffer). `VIDEO_SERVICES` carries every
    // field either way.
    if !is_video_initialized() {
        register_video_services(&VIDEO_SERVICES);
    }

    if let (Some(base), Some(info)) = (
        framebuffer::get_fb_base_ptr(),
        framebuffer::get_display_info(),
    ) {
        slopos_drivers::tty::vconsole::register_framebuffer(
            base,
            info.pitch,
            info.width,
            info.height,
            info.bytes_per_pixel(),
        );
    }

    if let Some(g) = ctx.gpu_control {
        register_gpu_control(g.available, g.set_image, g.move_cursor, g.set_mode);
    }

    let width = ctx.fb.info.width as i32;
    let height = ctx.fb.info.height as i32;
    if width > 0 && height > 0 {
        slopos_drivers::mouse::set_bounds(width, height);
    }

    scanout::set_current_framebuffer(ctx.fb);
    klog_info!(
        "scanout: adopted {}x{} pitch {}",
        ctx.fb.info.width,
        ctx.fb.info.height,
        ctx.fb.info.pitch,
    );
    show_splash();
    true
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
