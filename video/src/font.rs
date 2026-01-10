use core::ffi::{c_char, c_int};

use slopos_abi::DrawTarget;
use slopos_abi::font_render;

use crate::framebuffer;
use crate::graphics::GraphicsContext;

pub use slopos_abi::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH};

const FONT_SUCCESS: c_int = 0;
const FONT_ERROR_NO_FB: c_int = -1;
const FONT_ERROR_INVALID: c_int = -3;

fn framebuffer_ready() -> bool {
    framebuffer::framebuffer_is_initialized() != 0
}

fn c_str_to_slice(ptr: *const c_char) -> &'static [u8] {
    if ptr.is_null() {
        return &[];
    }
    let mut len = 0usize;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        core::slice::from_raw_parts(ptr as *const u8, len)
    }
}

pub fn draw_char(ctx: &mut GraphicsContext, x: i32, y: i32, ch: u8, fg: u32, bg: u32) {
    font_render::draw_char(ctx, x, y, ch, fg, bg);
}

pub fn draw_string(ctx: &mut GraphicsContext, x: i32, y: i32, text: &[u8], fg: u32, bg: u32) {
    font_render::draw_string(ctx, x, y, text, fg, bg);
}

pub fn draw_str(ctx: &mut GraphicsContext, x: i32, y: i32, text: &str, fg: u32, bg: u32) {
    font_render::draw_str(ctx, x, y, text, fg, bg);
}

pub fn string_width(text: &[u8]) -> i32 {
    font_render::string_width(text)
}

pub fn string_lines(text: &[u8]) -> i32 {
    font_render::string_lines(text)
}

pub fn font_draw_char_ctx(
    ctx: &GraphicsContext,
    x: i32,
    y: i32,
    c: c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    if !framebuffer_ready() {
        return FONT_ERROR_NO_FB;
    }

    let mut ctx_copy = match GraphicsContext::new() {
        Ok(c) => c,
        Err(_) => return FONT_ERROR_NO_FB,
    };
    let _ = ctx;

    let fmt = ctx_copy.pixel_format();
    let fg_raw = fmt.convert_color(fg_color);
    let bg_raw = fmt.convert_color(bg_color);

    let glyph = slopos_abi::font::get_glyph_or_space(c as u8);

    for (row_idx, &row_bits) in glyph.iter().enumerate() {
        let py = y + row_idx as i32;
        for col in 0..FONT_CHAR_WIDTH {
            let px = x + col;
            let is_fg = (row_bits & (0x80 >> col)) != 0;
            if is_fg {
                ctx_copy.draw_pixel(px, py, fg_raw);
            } else if bg_color != 0 {
                ctx_copy.draw_pixel(px, py, bg_raw);
            }
        }
    }

    FONT_SUCCESS
}

pub fn font_draw_string_ctx(
    ctx: &GraphicsContext,
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    if str_ptr.is_null() {
        return FONT_ERROR_INVALID;
    }
    if !framebuffer_ready() {
        return FONT_ERROR_NO_FB;
    }

    let mut ctx_copy = match GraphicsContext::new() {
        Ok(c) => c,
        Err(_) => return FONT_ERROR_NO_FB,
    };
    let _ = ctx;

    let fb_w = ctx_copy.width() as i32;
    let fb_h = ctx_copy.height() as i32;
    let text = c_str_to_slice(str_ptr);

    let fmt = ctx_copy.pixel_format();
    let fg_raw = fmt.convert_color(fg_color);
    let bg_raw = fmt.convert_color(bg_color);

    let mut cx = x;
    let mut cy = y;

    for &ch in text {
        match ch {
            b'\n' => {
                cx = x;
                cy += FONT_CHAR_HEIGHT;
            }
            b'\r' => {
                cx = x;
            }
            b'\t' => {
                let tab_width = 4 * FONT_CHAR_WIDTH;
                cx = ((cx - x + tab_width) / tab_width) * tab_width + x;
            }
            _ => {
                let glyph = slopos_abi::font::get_glyph_or_space(ch);
                for (row_idx, &row_bits) in glyph.iter().enumerate() {
                    let py = cy + row_idx as i32;
                    if py < 0 || py >= fb_h {
                        continue;
                    }
                    for col in 0..FONT_CHAR_WIDTH {
                        let px = cx + col;
                        if px < 0 || px >= fb_w {
                            continue;
                        }
                        let is_fg = (row_bits & (0x80 >> col)) != 0;
                        if is_fg {
                            ctx_copy.draw_pixel(px, py, fg_raw);
                        } else if bg_color != 0 {
                            ctx_copy.draw_pixel(px, py, bg_raw);
                        }
                    }
                }
                cx += FONT_CHAR_WIDTH;
                if cx + FONT_CHAR_WIDTH > fb_w {
                    cx = x;
                    cy += FONT_CHAR_HEIGHT;
                }
            }
        }
        if cy >= fb_h {
            break;
        }
    }

    FONT_SUCCESS
}
