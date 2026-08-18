//! Font loading and text rendering for appkit widgets.

mod loader;

use std::sync::OnceLock;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};
use slopos_font::atlas::GlyphAtlas;

const FONT_SIZE_PX: u16 = 16;

static ATLAS: OnceLock<Option<GlyphAtlas>> = OnceLock::new();

fn atlas() -> Option<&'static GlyphAtlas> {
    ATLAS
        .get_or_init(|| {
            let font_data = loader::load_font("mono")?;
            GlyphAtlas::new_with_source(
                font_data,
                FONT_SIZE_PX,
                slopos_font::FontSource::Filesystem,
            )
        })
        .as_ref()
}

const FALLBACK_CELL_W: i32 = 8;
const FALLBACK_CELL_H: i32 = 16;

pub fn cell_width() -> i32 {
    atlas().map_or(FALLBACK_CELL_W, |a| a.cell_width())
}

pub fn cell_height() -> i32 {
    atlas().map_or(FALLBACK_CELL_H, |a| a.cell_height())
}

pub fn draw_char<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    ch: u8,
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    atlas()?.draw_char(target, x, y, ch as u32, fg, bg)
}

pub fn draw_string<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &str,
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    atlas()?.draw_str(target, x, y, text, fg, bg)
}

pub fn draw_str_clipped<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &str,
    fg: Color32,
    bg: Color32,
    clip: &DamageRect,
) {
    if let Some(a) = atlas() {
        a.draw_str_clipped(target, x, y, text, fg, bg, clip);
    }
}

pub fn draw_char_clipped<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    ch: u8,
    fg: Color32,
    bg: Color32,
    clip: &DamageRect,
) {
    if let Some(a) = atlas() {
        a.draw_char_clipped(target, x, y, ch as u32, fg, bg, clip);
    }
}

pub fn string_width(text: &str) -> i32 {
    atlas().map_or(text.len() as i32 * FALLBACK_CELL_W, |a| a.str_width(text))
}

pub fn string_height(text: &str) -> i32 {
    atlas().map_or(FALLBACK_CELL_H, |a| {
        a.bytes_lines(text.as_bytes()) * a.cell_height()
    })
}
