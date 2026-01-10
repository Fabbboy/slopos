//! Font rendering for DrawTarget surfaces
//!
//! Uses the bitmap font data from font.rs and renders generically
//! to any DrawTarget implementation.

use crate::draw::DrawTarget;
use crate::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, get_glyph_or_space};

pub fn draw_char<T: DrawTarget>(target: &mut T, x: i32, y: i32, ch: u8, fg: u32, bg: u32) {
    let fmt = target.pixel_format();
    let fg_raw = fmt.convert_color(fg);
    let bg_raw = fmt.convert_color(bg);
    let glyph = get_glyph_or_space(ch);

    for (row_idx, &row_bits) in glyph.iter().enumerate() {
        let py = y + row_idx as i32;
        for col in 0..FONT_CHAR_WIDTH {
            let px = x + col;
            let is_fg = (row_bits & (0x80 >> col)) != 0;
            if is_fg {
                target.draw_pixel(px, py, fg_raw);
            } else if bg != 0 {
                target.draw_pixel(px, py, bg_raw);
            }
        }
    }
}

pub fn draw_string<T: DrawTarget>(target: &mut T, x: i32, y: i32, text: &[u8], fg: u32, bg: u32) {
    let w = target.width() as i32;
    let h = target.height() as i32;
    let mut cx = x;
    let mut cy = y;

    for &ch in text {
        match ch {
            0 => break,
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
                draw_char(target, cx, cy, ch, fg, bg);
                cx += FONT_CHAR_WIDTH;
                if cx + FONT_CHAR_WIDTH > w {
                    cx = x;
                    cy += FONT_CHAR_HEIGHT;
                }
            }
        }
        if cy >= h {
            break;
        }
    }
}

#[inline]
pub fn draw_str<T: DrawTarget>(target: &mut T, x: i32, y: i32, text: &str, fg: u32, bg: u32) {
    draw_string(target, x, y, text.as_bytes(), fg, bg);
}

pub fn string_width(text: &[u8]) -> i32 {
    let mut width = 0i32;
    for &ch in text {
        match ch {
            0 | b'\n' => break,
            b'\t' => {
                let tab_width = 4 * FONT_CHAR_WIDTH;
                width = ((width + tab_width - 1) / tab_width) * tab_width;
            }
            _ => width += FONT_CHAR_WIDTH,
        }
    }
    width
}

pub fn string_lines(text: &[u8]) -> i32 {
    let mut lines = 1i32;
    for &ch in text {
        if ch == 0 {
            break;
        }
        if ch == b'\n' {
            lines += 1;
        }
    }
    lines
}

pub fn str_width(text: &str) -> i32 {
    string_width(text.as_bytes())
}

pub fn str_lines(text: &str) -> i32 {
    string_lines(text.as_bytes())
}
