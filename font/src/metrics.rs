//! Text measurement utilities.

use crate::ttf_parser::TtfFont;

/// Measure the width and height of a text string at a given pixel size.
///
/// Returns `(width, height)` in pixels. Multi-line text is not handled;
/// this measures a single line.
pub fn measure_text(font: &TtfFont<'_>, text: &str, size_px: u16) -> (i32, i32) {
    let upem = font.units_per_em() as f32;
    if upem == 0.0 {
        return (0, 0);
    }
    let scale = size_px as f32 / upem;

    let hhea = font.hhea();
    let height = libm::ceilf((hhea.ascender as f32 - hhea.descender as f32) * scale) as i32;

    // Accumulate width in float to avoid per-character truncation error.
    let mut width_f = 0.0f32;

    for ch in text.chars() {
        if let Some(glyph_id) = font.glyph_index(ch as u32) {
            if let Some(hm) = font.h_metrics(glyph_id) {
                width_f += hm.advance_width as f32 * scale;
            }
        }
    }

    (libm::ceilf(width_f) as i32, height.max(1))
}
