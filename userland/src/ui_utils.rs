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
    gfx::font::draw_string(buf, x + size / 4, y + size / 4, label, COLOR_TEXT, color);
}
