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
    let height = ((hhea.ascender as i32 - hhea.descender as i32) as f32 * scale) as i32;

    let mut width = 0i32;

    for ch in text.chars() {
        if let Some(glyph_id) = font.glyph_index(ch as u32) {
            if let Some(hm) = font.h_metrics(glyph_id) {
                width += (hm.advance_width as f32 * scale) as i32;
            }
        }
    }

    (width, height.max(1))
}
