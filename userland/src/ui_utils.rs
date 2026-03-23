use crate::gfx::{self, DrawBuffer};
use crate::theme::*;

pub fn draw_button(
    buf: &mut DrawBuffer,
    x: i32,
    y: i32,
    size: i32,
    label: &str,
    hover: bool,
    is_close: bool,
) {
    let color = if hover && is_close {
        COLOR_BUTTON_CLOSE_HOVER
    } else if hover {
        COLOR_BUTTON_HOVER
    } else {
        COLOR_BUTTON
    };

    gfx::fill_rect(buf, x, y, size, size, color);
    let tw = crate::gfx::font::string_width(label);
    let th = crate::gfx::font::cell_height();
    let tx = x + (size - tw) / 2;
    let ty = y + (size - th) / 2;
    crate::gfx::font::draw_string(buf, tx, ty, label, COLOR_TEXT, color);
}
