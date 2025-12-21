#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

use core::ffi::{c_char, c_int};
use slopos_drivers::serial_println;
use slopos_drivers::video_bridge::{self, VideoCallbacks, VideoResult};
use slopos_lib::FramebufferInfo;

pub mod font;
pub mod framebuffer;
pub mod graphics;
pub mod roulette_core;
pub mod surface;
pub mod splash;

fn draw_rect_filled_fast_bridge(x: i32, y: i32, w: i32, h: i32, color: u32) -> c_int {
    video_result_to_code(graphics::graphics_draw_rect_filled_fast_status(x, y, w, h, color))
}

fn draw_line_bridge(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) -> c_int {
    video_result_to_code(graphics::graphics_draw_line_status(x0, y0, x1, y1, color))
}

fn draw_circle_bridge(cx: i32, cy: i32, radius: i32, color: u32) -> c_int {
    video_result_to_code(graphics::graphics_draw_circle_status(cx, cy, radius, color))
}

fn draw_circle_filled_bridge(cx: i32, cy: i32, radius: i32, color: u32) -> c_int {
    video_result_to_code(graphics::graphics_draw_circle_filled_status(
        cx, cy, radius, color,
    ))
}

fn font_draw_string_bridge(
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    font::font_draw_string(x, y, str_ptr, fg_color, bg_color)
}

fn framebuffer_blit_bridge(
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> c_int {
    framebuffer::framebuffer_blit(src_x, src_y, dst_x, dst_y, width, height)
}

fn framebuffer_get_info_bridge() -> *mut slopos_drivers::video_bridge::FramebufferInfoC {
    framebuffer::framebuffer_get_info()
        as *mut slopos_drivers::video_bridge::FramebufferInfoC
}

fn roulette_draw_bridge(fate: u32) -> c_int {
    video_result_to_code(roulette_core::roulette_draw_kernel(fate))
}

fn surface_draw_rect_filled_fast_bridge(
    task_id: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) -> c_int {
    video_result_to_code(surface::surface_draw_rect_filled_fast(task_id, x, y, w, h, color))
}

fn surface_draw_line_bridge(
    task_id: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> c_int {
    video_result_to_code(surface::surface_draw_line(task_id, x0, y0, x1, y1, color))
}

fn surface_draw_circle_bridge(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> c_int {
    video_result_to_code(surface::surface_draw_circle(task_id, cx, cy, radius, color))
}

fn surface_draw_circle_filled_bridge(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> c_int {
    video_result_to_code(surface::surface_draw_circle_filled(task_id, cx, cy, radius, color))
}

fn surface_font_draw_string_bridge(
    task_id: u32,
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    surface::surface_font_draw_string(task_id, x, y, str_ptr, fg_color, bg_color)
}

fn surface_blit_bridge(
    task_id: u32,
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> c_int {
    video_result_to_code(surface::surface_blit(
        task_id, src_x, src_y, dst_x, dst_y, width, height,
    ))
}

fn compositor_present_bridge() -> c_int {
    surface::compositor_present()
}

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

        video_bridge::register_video_callbacks(VideoCallbacks {
            draw_rect_filled_fast: Some(draw_rect_filled_fast_bridge),
            draw_line: Some(draw_line_bridge),
            draw_circle: Some(draw_circle_bridge),
            draw_circle_filled: Some(draw_circle_filled_bridge),
            font_draw_string: Some(font_draw_string_bridge),
            framebuffer_blit: Some(framebuffer_blit_bridge),
            framebuffer_get_info: Some(framebuffer_get_info_bridge),
            roulette_draw: Some(roulette_draw_bridge),
            surface_draw_rect_filled_fast: Some(surface_draw_rect_filled_fast_bridge),
            surface_draw_line: Some(surface_draw_line_bridge),
            surface_draw_circle: Some(surface_draw_circle_bridge),
            surface_draw_circle_filled: Some(surface_draw_circle_filled_bridge),
            surface_font_draw_string: Some(surface_font_draw_string_bridge),
            surface_blit: Some(surface_blit_bridge),
            compositor_present: Some(compositor_present_bridge),
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
