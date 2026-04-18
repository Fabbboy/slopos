//! TrueType font rasterizer for SlopOS.
//!
//! Provides anti-aliased text rendering from `.ttf` font files. Parses
//! TrueType outline data, rasterizes glyphs with coverage-based anti-aliasing,
//! and caches rendered glyphs for performance.
//!
//! # Usage
//!
//! ```ignore
//! let font_data: &[u8] = /* load .ttf file */;
//! let mut renderer = FontRenderer::new(font_data).unwrap();
//! renderer.draw_text(&mut canvas, 10, 20, "Hello", 16, Color32::WHITE, Color32::BLACK);
//! ```

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod atlas;
pub mod bitmap;
pub mod cache;
pub mod metrics;
pub mod outline;
pub mod rasterizer;
pub mod ttf_parser;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

// ── Shared ASCII constants ─────────────────────────────────────────────
/// First printable ASCII codepoint (space).
pub const ASCII_FIRST: u32 = 0x20;
/// Last printable ASCII codepoint (tilde).
pub const ASCII_LAST: u32 = 0x7E;
/// Number of printable ASCII characters (0x20..=0x7E → 95).
pub const ASCII_COUNT: usize = (ASCII_LAST - ASCII_FIRST + 1) as usize;

use cache::GlyphCache;
use outline::outline_to_edges;
use rasterizer::{RasterizedGlyph, rasterize};
use ttf_parser::TtfFont;

/// Describes the origin of font data for diagnostics and fallback logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontSource {
    /// Compiled into the binary via `include_bytes!`.
    Embedded,
    /// Loaded from the filesystem at runtime.
    Filesystem,
    /// Loaded via `SYS_FONT_SET` from userland-provided bitmap or coverage data.
    Syscall,
    /// Minimal bitmap fallback (VGA 8\u{d7}16) used when no other font is available.
    BitmapFallback,
}

/// High-level font renderer that combines parsing, rasterization, and caching.
pub struct FontRenderer<'a> {
    pub(crate) font: TtfFont<'a>,
    cache: GlyphCache,
    source: FontSource,
}

impl<'a> FontRenderer<'a> {
    /// Create a new font renderer from raw TrueType font data.
    ///
    /// Returns `None` if the font data cannot be parsed.
    /// Source defaults to [`FontSource::Embedded`].
    pub fn new(ttf_data: &'a [u8]) -> Option<Self> {
        Self::new_with_source(ttf_data, FontSource::Embedded)
    }

    /// Create a new font renderer with an explicit source tag.
    pub fn new_with_source(ttf_data: &'a [u8], source: FontSource) -> Option<Self> {
        let font = TtfFont::parse(ttf_data)?;
        Some(Self {
            font,
            cache: GlyphCache::new(),
            source,
        })
    }

    /// Returns the source from which this font was loaded.
    pub fn source(&self) -> FontSource {
        self.source
    }

    /// Draw a text string onto a canvas at the given position.
    ///
    /// `(x, y)` is the top-left corner of the text. `size_px` is the font
    /// size in pixels. `bg` is the background colour for anti-alias blending;
    /// pass an opaque colour for MMIO/write-only surfaces, or
    /// `Color32::TRANSPARENT` to composite over existing content (requires a
    /// readable surface).  Returns the damage rectangle covering all rendered
    /// glyphs.
    pub fn draw_text<T: Canvas>(
        &mut self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        size_px: u16,
        color: Color32,
        bg: Color32,
    ) -> Option<DamageRect> {
        let upem = self.font.units_per_em() as f32;
        if upem == 0.0 {
            return None;
        }
        let scale = size_px as f32 / upem;

        let hhea = self.font.hhea();
        let ascender = libm::roundf(hhea.ascender as f32 * scale) as i32;

        let mut cursor_x = x as f32;
        let mut damage: Option<DamageRect> = None;

        for ch in text.chars() {
            let codepoint = ch as u32;

            // Try cache first
            if self.cache.get(codepoint, size_px).is_none() {
                // Rasterize and cache
                if let Some(glyph) = self.rasterize_glyph(codepoint, size_px, scale, ascender) {
                    self.cache.insert(codepoint, size_px, glyph);
                }
            }

            // Get the true advance from font metrics (full float precision).
            let true_advance = self
                .font
                .glyph_index(codepoint)
                .and_then(|gid| self.font.h_metrics(gid))
                .map(|hm| hm.advance_width as f32 * scale)
                .unwrap_or(size_px as f32 * 0.5);

            // Extract glyph info from cache to avoid overlapping borrows
            let glyph_info = self.cache.get(codepoint, size_px).map(|g| {
                (
                    g.bearing_x,
                    g.bearing_y,
                    g.width,
                    g.height,
                    g.coverage.clone(),
                )
            });

            if let Some((bearing_x, bearing_y, gw, gh, cov)) = glyph_info {
                let gx = libm::roundf(cursor_x) as i32 + bearing_x as i32;
                let gy = y + ascender - bearing_y as i32;

                let glyph_damage = Self::draw_glyph_coverage_static(
                    target, gx, gy, gw as i32, gh as i32, &cov, color, bg,
                );

                damage = match (damage, glyph_damage) {
                    (Some(d), Some(g)) => Some(DamageRect {
                        x0: d.x0.min(g.x0),
                        y0: d.y0.min(g.y0),
                        x1: d.x1.max(g.x1),
                        y1: d.y1.max(g.y1),
                    }),
                    (None, g) => g,
                    (d, None) => d,
                };
            }

            cursor_x += true_advance;
        }

        if let Some(d) = damage {
            target.report_damage(d);
        }
        damage
    }

    /// Measure the width and height of a text string at the given size.
    pub fn measure_text(&self, text: &str, size_px: u16) -> (i32, i32) {
        metrics::measure_text(&self.font, text, size_px)
    }

    /// Rasterize a single glyph at the given size.
    pub(crate) fn rasterize_glyph(
        &self,
        codepoint: u32,
        _size_px: u16,
        scale: f32,
        _ascender: i32,
    ) -> Option<RasterizedGlyph> {
        let glyph_id = self.font.glyph_index(codepoint)?;
        if glyph_id == 0 && codepoint != 0 {
            // .notdef for non-null codepoint
            return None;
        }

        let hm = self.font.h_metrics(glyph_id)?;
        let outline = self.font.glyph_outline(glyph_id);

        let scaled_advance = libm::roundf(hm.advance_width as f32 * scale) as u16;

        match outline {
            Some(ref out) if !out.contours.is_empty() => {
                // +1 for anti-aliasing margin (ceilf already rounds up).
                let glyph_width =
                    libm::ceilf((out.x_max as f32 - out.x_min as f32) * scale) as i32 + 1;
                let glyph_height =
                    libm::ceilf((out.y_max as f32 - out.y_min as f32) * scale) as i32 + 1;

                if glyph_width <= 0 || glyph_height <= 0 {
                    return None;
                }

                // Use left_side_bearing from hmtx (the font's intended spacing),
                // not x_min from the outline (visual bbox edge).
                let bearing_x = libm::roundf(hm.left_side_bearing as f32 * scale) as i16;
                let bearing_y = libm::roundf(out.y_max as f32 * scale) as i16;

                // Offset edges so the glyph bitmap starts at pixel (0, 0).
                let x_offset = -(out.x_min as f32 * scale);
                let y_offset = out.y_max as f32 * scale;

                let edges = outline_to_edges(out, scale, y_offset);

                // Shift edges by x_offset
                let shifted_edges: slopos_alloc::KVec<outline::Edge> =
                    slopos_alloc::KVec::from_iter_fallible(edges.iter().map(|e| outline::Edge {
                        x0: e.x0 + x_offset,
                        y0: e.y0,
                        x1: e.x1 + x_offset,
                        y1: e.y1,
                    }))
                    .expect("shifted_edges: alloc");

                let coverage =
                    rasterize(&shifted_edges, glyph_width as usize, glyph_height as usize);

                Some(RasterizedGlyph {
                    width: glyph_width as u16,
                    height: glyph_height as u16,
                    bearing_x,
                    bearing_y,
                    advance: scaled_advance,
                    coverage,
                })
            }
            _ => {
                // Space or empty glyph — no coverage data, just advance
                Some(RasterizedGlyph {
                    width: 0,
                    height: 0,
                    bearing_x: 0,
                    bearing_y: 0,
                    advance: scaled_advance,
                    coverage: slopos_alloc::KVec::new(),
                })
            }
        }
    }

    /// Draw a coverage bitmap onto a canvas.
    ///
    /// When `bg` is opaque, blends fg/bg directly (no framebuffer read-back,
    /// safe for MMIO).  When `bg` is transparent, composites onto the
    /// existing pixel (requires a readable surface like `DrawBuffer`).
    fn draw_glyph_coverage_static<T: Canvas>(
        target: &mut T,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        coverage: &[u8],
        color: Color32,
        bg: Color32,
    ) -> Option<DamageRect> {
        if w <= 0 || h <= 0 || coverage.is_empty() {
            return None;
        }

        let buf_w = target.width() as i32;
        let buf_h = target.height() as i32;

        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w - 1).min(buf_w - 1);
        let y1 = (y + h - 1).min(buf_h - 1);

        if x0 > x1 || y0 > y1 {
            return None;
        }

        let has_bg = bg.0 != 0;
        let blend_bg = if has_bg { bg } else { Color32::BLACK };
        let fmt = target.pixel_format();

        for row in y0..=y1 {
            for col in x0..=x1 {
                let cov_x = (col - x) as usize;
                let cov_y = (row - y) as usize;
                let idx = cov_y * (w as usize) + cov_x;

                if idx < coverage.len() {
                    let cov = coverage[idx];
                    if cov == 0 {
                        continue;
                    }
                    if cov == 255 {
                        target.put_pixel(col, row, fmt.encode(color));
                    } else {
                        let blended = atlas::blend_color32(cov, color, blend_bg);
                        target.put_pixel(col, row, fmt.encode(blended));
                    }
                }
            }
        }

        Some(DamageRect { x0, y0, x1, y1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::GlyphAtlas;
    use slopos_abi::draw::{Canvas, EncodedPixel};
    use slopos_abi::pixel::PixelFormat;

    const INTER_TTF: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../assets/fonts/Inter-Regular.ttf"
    ));

    struct TestCanvas {
        data: slopos_alloc::KVec<u8>,
        width: u32,
        height: u32,
    }

    impl TestCanvas {
        fn new(width: u32, height: u32) -> Self {
            Self {
                data: slopos_alloc::KVec::<u8>::zeroed((width * height * 4) as usize)
                    .expect("test alloc"),
                width,
                height,
            }
        }

        fn has_any_nonzero(&self) -> bool {
            self.data.iter().any(|&b| b != 0)
        }

        fn read_pixel(&self, x: u32, y: u32) -> Color32 {
            let off = (y * self.width + x) as usize * 4;
            let raw = u32::from_le_bytes([
                self.data[off],
                self.data[off + 1],
                self.data[off + 2],
                self.data[off + 3],
            ]);
            PixelFormat::Argb8888.decode(raw)
        }
    }

    impl Canvas for TestCanvas {
        fn width(&self) -> u32 {
            self.width
        }
        fn height(&self) -> u32 {
            self.height
        }
        fn pitch_bytes(&self) -> usize {
            self.width as usize * 4
        }
        fn bytes_per_pixel(&self) -> u8 {
            4
        }
        fn pixel_format(&self) -> PixelFormat {
            PixelFormat::Argb8888
        }
        fn write_encoded_at(&mut self, off: usize, pixel: EncodedPixel) {
            let bytes = pixel.to_u32().to_le_bytes();
            if off + 4 <= self.data.len() {
                self.data[off..off + 4].copy_from_slice(&bytes);
            }
        }
        fn read_encoded_at(&self, off: usize) -> u32 {
            if off + 4 <= self.data.len() {
                u32::from_le_bytes([
                    self.data[off],
                    self.data[off + 1],
                    self.data[off + 2],
                    self.data[off + 3],
                ])
            } else {
                0
            }
        }
    }

    #[test]
    fn parse_inter_font() {
        let font = ttf_parser::TtfFont::parse(INTER_TTF).unwrap();
        assert!(font.units_per_em() > 0);
        assert!(font.num_glyphs() > 100);
    }

    #[test]
    fn glyph_index_ascii() {
        let font = ttf_parser::TtfFont::parse(INTER_TTF).unwrap();
        let gid = font.glyph_index('A' as u32).unwrap();
        assert!(gid > 0);
        assert!(font.glyph_index(' ' as u32).is_some());
    }

    #[test]
    fn glyph_outline_has_contours() {
        let font = ttf_parser::TtfFont::parse(INTER_TTF).unwrap();
        let gid = font.glyph_index('A' as u32).unwrap();
        let outline = font.glyph_outline(gid).unwrap();
        assert!(!outline.contours.is_empty());
    }

    #[test]
    fn h_metrics_for_ascii() {
        let font = ttf_parser::TtfFont::parse(INTER_TTF).unwrap();
        let gid = font.glyph_index('A' as u32).unwrap();
        let hm = font.h_metrics(gid).unwrap();
        assert!(hm.advance_width > 0);
    }

    #[test]
    fn measure_text_nonzero() {
        let renderer = FontRenderer::new(INTER_TTF).unwrap();
        let (w, h) = renderer.measure_text("Hello", 16);
        assert!(w > 0 && h > 0, "w={w}, h={h}");
    }

    #[test]
    fn measure_empty_text_zero_width() {
        let renderer = FontRenderer::new(INTER_TTF).unwrap();
        let (w, _) = renderer.measure_text("", 16);
        assert_eq!(w, 0);
    }

    #[test]
    fn draw_text_produces_pixels() {
        let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
        let mut canvas = TestCanvas::new(200, 50);
        let d = renderer.draw_text(
            &mut canvas,
            10,
            10,
            "Hello",
            16,
            Color32::WHITE,
            Color32::BLACK,
        );
        assert!(d.is_some());
        assert!(canvas.has_any_nonzero());
    }

    #[test]
    fn draw_text_damage_rect_sane() {
        let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
        let mut canvas = TestCanvas::new(200, 50);
        let d = renderer
            .draw_text(
                &mut canvas,
                10,
                10,
                "AB",
                16,
                Color32::WHITE,
                Color32::BLACK,
            )
            .unwrap();
        assert!(d.x0 >= 0 && d.y0 >= 0 && d.x1 < 200 && d.y1 < 50);
        assert!(d.x1 - d.x0 > 5, "width {} too small", d.x1 - d.x0);
    }

    #[test]
    fn rasterizer_full_coverage_reaches_255() {
        let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
        let mut canvas = TestCanvas::new(100, 100);
        renderer.draw_text(&mut canvas, 10, 10, "O", 48, Color32::WHITE, Color32::BLACK);
        let max_a = (0..100u32)
            .flat_map(|y| (0..100u32).map(move |x| (x, y)))
            .map(|(x, y)| canvas.read_pixel(x, y).alpha())
            .max()
            .unwrap_or(0);
        assert_eq!(max_a, 255, "max coverage={max_a}, expected 255");
    }

    #[test]
    fn different_sizes_produce_different_metrics() {
        let r1 = FontRenderer::new(INTER_TTF).unwrap();
        let r2 = FontRenderer::new(INTER_TTF).unwrap();
        let (w1, h1) = r1.measure_text("A", 12);
        let (w2, h2) = r2.measure_text("A", 24);
        assert!(w2 > w1, "24px should be wider than 12px");
        assert!(h2 > h1, "24px should be taller than 12px");
    }

    #[test]
    fn space_advances_cursor() {
        let renderer = FontRenderer::new(INTER_TTF).unwrap();
        let (w_a, _) = renderer.measure_text("A", 16);
        let (w_aa, _) = renderer.measure_text("A A", 16);
        assert!(w_aa > w_a * 2 - 5, "A={w_a}, A_A={w_aa}");
    }

    #[test]
    fn font_source_default_is_embedded() {
        let r = FontRenderer::new(INTER_TTF).unwrap();
        assert_eq!(r.source(), FontSource::Embedded);
    }

    #[test]
    fn font_source_filesystem_tracking() {
        let r = FontRenderer::new_with_source(INTER_TTF, FontSource::Filesystem).unwrap();
        assert_eq!(r.source(), FontSource::Filesystem);
    }

    #[test]
    fn atlas_source_default_is_embedded() {
        let atlas = GlyphAtlas::new(INTER_TTF, 16).unwrap();
        assert_eq!(atlas.source(), FontSource::Embedded);
    }

    #[test]
    fn atlas_source_with_override() {
        let atlas = GlyphAtlas::new_with_source(INTER_TTF, 16, FontSource::Filesystem).unwrap();
        assert_eq!(atlas.source(), FontSource::Filesystem);
    }

    #[test]
    fn bitmap_font_data_has_correct_size() {
        assert_eq!(
            bitmap::VGA_FONT_8X16.len(),
            bitmap::BITMAP_FONT_GLYPH_COUNT * bitmap::BITMAP_FONT_BYTES_PER_GLYPH
        );
    }

    #[test]
    fn bitmap_glyph_a_is_nonzero() {
        let glyph = bitmap::glyph_bitmap(b'A');
        let nonzero = glyph.iter().any(|&b| b != 0);
        assert!(nonzero, "glyph 'A' should have non-zero pixels");
    }

    #[test]
    fn bitmap_render_produces_coverage() {
        let mut buf = [0u8; 128]; // 8 * 16
        let (w, h) = bitmap::render_bitmap_glyph(b'A', &mut buf);
        assert_eq!(w, 8);
        assert_eq!(h, 16);
        let has_pixels = buf.iter().any(|&b| b == 255);
        assert!(has_pixels, "rendered 'A' should have foreground pixels");
    }

    #[test]
    fn bitmap_space_is_blank() {
        let glyph = bitmap::glyph_bitmap(b' ');
        assert!(
            glyph.iter().all(|&b| b == 0),
            "space glyph should be all zeros"
        );
    }
}
