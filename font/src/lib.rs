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
//! renderer.draw_text(&mut canvas, 10, 20, "Hello", 16, Color32::WHITE);
//! ```

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod cache;
pub mod metrics;
pub mod outline;
pub mod rasterizer;
pub mod ttf_parser;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

use cache::GlyphCache;
use outline::outline_to_edges;
use rasterizer::{RasterizedGlyph, rasterize};
use ttf_parser::TtfFont;

/// High-level font renderer that combines parsing, rasterization, and caching.
pub struct FontRenderer<'a> {
    font: TtfFont<'a>,
    cache: GlyphCache,
}

impl<'a> FontRenderer<'a> {
    /// Create a new font renderer from raw TrueType font data.
    ///
    /// Returns `None` if the font data cannot be parsed.
    pub fn new(ttf_data: &'a [u8]) -> Option<Self> {
        let font = TtfFont::parse(ttf_data)?;
        Some(Self {
            font,
            cache: GlyphCache::new(),
        })
    }

    /// Draw a text string onto a canvas at the given position.
    ///
    /// `(x, y)` is the top-left corner of the text. `size_px` is the font
    /// size in pixels. Returns the damage rectangle covering all rendered
    /// glyphs.
    pub fn draw_text<T: Canvas>(
        &mut self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        size_px: u16,
        color: Color32,
    ) -> Option<DamageRect> {
        let upem = self.font.units_per_em() as f32;
        if upem == 0.0 {
            return None;
        }
        let scale = size_px as f32 / upem;

        let hhea = self.font.hhea();
        let ascender = (hhea.ascender as f32 * scale) as i32;

        let mut cursor_x = x;
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

            // Extract glyph info from cache to avoid overlapping borrows
            let glyph_info = self.cache.get(codepoint, size_px).map(|g| {
                (
                    g.bearing_x,
                    g.bearing_y,
                    g.width,
                    g.height,
                    g.advance,
                    g.coverage.clone(),
                )
            });

            if let Some((bearing_x, bearing_y, gw, gh, advance, cov)) = glyph_info {
                let gx = cursor_x + bearing_x as i32;
                let gy = y + ascender - bearing_y as i32;

                let glyph_damage = Self::draw_glyph_coverage_static(
                    target, gx, gy, gw as i32, gh as i32, &cov, color,
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

                cursor_x += advance as i32;
            } else {
                // Fallback: skip unknown glyphs with a small advance
                cursor_x += (size_px / 2) as i32;
            }
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
    fn rasterize_glyph(
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

        match outline {
            Some(ref out) if !out.contours.is_empty() => {
                let glyph_width =
                    libm::ceilf((out.x_max as i32 - out.x_min as i32) as f32 * scale) as i32 + 2;
                let glyph_height =
                    libm::ceilf((out.y_max as i32 - out.y_min as i32) as f32 * scale) as i32 + 2;

                if glyph_width <= 0 || glyph_height <= 0 {
                    return None;
                }

                let bearing_x = (out.x_min as f32 * scale) as i16;
                let bearing_y = (out.y_max as f32 * scale) as i16;

                // Offset edges so the glyph starts at pixel (0, 0)
                let x_offset = -(out.x_min as f32 * scale);
                let y_offset = out.y_max as f32 * scale;

                let edges = outline_to_edges(out, scale, y_offset);

                // Shift edges by x_offset
                let shifted_edges: alloc::vec::Vec<_> = edges
                    .iter()
                    .map(|e| outline::Edge {
                        x0: e.x0 + x_offset,
                        y0: e.y0,
                        x1: e.x1 + x_offset,
                        y1: e.y1,
                    })
                    .collect();

                let coverage =
                    rasterize(&shifted_edges, glyph_width as usize, glyph_height as usize);

                Some(RasterizedGlyph {
                    width: glyph_width as u16,
                    height: glyph_height as u16,
                    bearing_x,
                    bearing_y,
                    advance: (hm.advance_width as f32 * scale) as u16,
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
                    advance: (hm.advance_width as f32 * scale) as u16,
                    coverage: alloc::vec::Vec::new(),
                })
            }
        }
    }

    /// Draw a coverage bitmap onto a canvas using alpha blending.
    fn draw_glyph_coverage_static<T: Canvas>(
        target: &mut T,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        coverage: &[u8],
        color: Color32,
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

        for row in y0..=y1 {
            for col in x0..=x1 {
                let cov_x = (col - x) as usize;
                let cov_y = (row - y) as usize;
                let idx = cov_y * (w as usize) + cov_x;

                if idx < coverage.len() {
                    let cov = coverage[idx];
                    if cov > 0 {
                        slopos_gfx::blend::put_pixel_coverage(target, col, row, color, cov);
                    }
                }
            }
        }

        Some(DamageRect {
            x0,
            y0,
            x1,
            y1,
        })
    }
}
