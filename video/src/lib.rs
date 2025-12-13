#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

use slopos_lib::FramebufferInfo;
use slopos_drivers::serial_println;

pub mod framebuffer;
pub mod graphics;
pub mod font;
pub mod splash;
pub mod roulette_core;

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

        if splash::splash_show_boot_screen() != 0 {
            serial_println!("Splash paint failed; falling back to banner stripe.");
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
