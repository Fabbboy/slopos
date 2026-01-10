use super::DrawBuffer;
use slopos_abi::font_render;

pub use slopos_abi::font::{
    FONT_CHAR_COUNT, FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, FONT_DATA, FONT_FIRST_CHAR, FONT_LAST_CHAR,
    get_glyph,
};

pub fn draw_char(buf: &mut DrawBuffer, x: i32, y: i32, ch: u8, fg: u32, bg: u32) {
    font_render::draw_char(buf, x, y, ch, fg, bg);
}

pub fn draw_string(buf: &mut DrawBuffer, x: i32, y: i32, text: &str, fg: u32, bg: u32) {
    let width = buf.width() as i32;
    let height = buf.height() as i32;

    font_render::draw_str(buf, x, y, text, fg, bg);

    let text_w = string_width(text);
    let text_h = string_height(text);
    let x1 = x.max(0);
    let y1 = y.max(0);
    let x2 = (x + text_w - 1).min(width - 1);
    let y2 = (y + text_h - 1).min(height - 1);

    if x1 <= x2 && y1 <= y2 {
        buf.add_damage(x1, y1, x2, y2);
    }
}

pub fn string_width(text: &str) -> i32 {
    font_render::str_width(text)
}

pub fn string_lines(text: &str) -> i32 {
    font_render::str_lines(text)
}

pub fn string_height(text: &str) -> i32 {
    string_lines(text) * FONT_CHAR_HEIGHT
}
