use std::sync::OnceLock;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};
use slopos_font::FontSource;
use slopos_font::atlas::GlyphAtlas;

use super::font_loader;

const FONT_SIZE_PX: u16 = 16;

static ATLAS: OnceLock<GlyphAtlas> = OnceLock::new();

fn atlas() -> &'static GlyphAtlas {
    ATLAS.get_or_init(|| {
        let (font_data, from_fs) =
            font_loader::load_font_or_embedded("mono", JETBRAINS_MONO_EMBEDDED);
        let source = if from_fs {
            FontSource::Filesystem
        } else {
            FontSource::Embedded
        };
        GlyphAtlas::new_with_source(font_data, FONT_SIZE_PX, source)
            .expect("font: failed to create glyph atlas")
    })
}

const JETBRAINS_MONO_EMBEDDED: &[u8] =
    include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");

pub fn cell_width() -> i32 {
    atlas().cell_width()
}

pub fn cell_height() -> i32 {
    atlas().cell_height()
}

pub fn draw_char<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    ch: u8,
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    atlas().draw_char(target, x, y, ch as u32, fg, bg)
}

pub fn draw_string<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &str,
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    atlas().draw_str(target, x, y, text, fg, bg)
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
    atlas().draw_str_clipped(target, x, y, text, fg, bg, clip);
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
    atlas().draw_char_clipped(target, x, y, ch as u32, fg, bg, clip);
}

pub fn string_width(text: &str) -> i32 {
    atlas().str_width(text)
}

pub fn string_height(text: &str) -> i32 {
    atlas().bytes_lines(text.as_bytes()) * cell_height()
}
