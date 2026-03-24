pub mod font;
pub mod font_loader;

pub use slopos_abi::Canvas;
pub use slopos_abi::damage::{self, DamageRect, MAX_DAMAGE_REGIONS};
pub use slopos_abi::pixel::PixelFormat;
pub use slopos_gfx::DrawBuffer;
pub use slopos_gfx::damage::DamageTracker;

pub use slopos_gfx::blend::{alpha_blend, fill_rect_blended, fill_rect_blended_clipped};
pub use slopos_gfx::canvas_ops::{
    circle as draw_circle, circle_filled as draw_circle_filled, fill_rect, fill_rect_clipped,
    line as draw_line, rect as draw_rect,
};

pub fn draw_char_clipped<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    ch: u8,
    fg: slopos_abi::draw::Color32,
    bg: slopos_abi::draw::Color32,
    clip: &DamageRect,
) {
    font::draw_char_clipped(target, x, y, ch, fg, bg, clip);
}

pub fn draw_str_clipped<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &str,
    fg: slopos_abi::draw::Color32,
    bg: slopos_abi::draw::Color32,
    clip: &DamageRect,
) {
    font::draw_str_clipped(target, x, y, text, fg, bg, clip);
}
