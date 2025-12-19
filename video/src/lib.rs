#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

use slopos_drivers::serial_println;
use slopos_drivers::video_bridge::{self, VideoCallbacks};
use slopos_lib::FramebufferInfo;

pub mod font;
pub mod framebuffer;
pub mod graphics;
pub mod roulette_core;
pub mod splash;

pub fn init(framebuffer: Option<FramebufferInfo>) {
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

        fn fb_info_bridge() -> *mut slopos_drivers::video_bridge::FramebufferInfoC {
            framebuffer::framebuffer_get_info() as *mut slopos_drivers::video_bridge::FramebufferInfoC
        }

        video_bridge::register_video_callbacks(VideoCallbacks {
            draw_rect_filled_fast: Some(graphics::graphics_draw_rect_filled_fast_status),
            draw_line: Some(graphics::graphics_draw_line_status),
            draw_circle: Some(graphics::graphics_draw_circle_status),
            draw_circle_filled: Some(graphics::graphics_draw_circle_filled_status),
            font_draw_string: Some(font::font_draw_string),
            framebuffer_get_info: Some(fb_info_bridge),
            roulette_draw: Some(roulette_core::roulette_draw_kernel),
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
