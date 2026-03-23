//! Pre-rasterized fixed-width glyph atlas for fast terminal/console rendering.
//!
//! Contains coverage bitmaps for ASCII 32-126 at a fixed cell size.
//! Each pixel has a coverage value 0-255 for anti-aliased rendering.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

use crate::FontRenderer;

const ASCII_FIRST: u32 = 32;
const ASCII_LAST: u32 = 126;
const ASCII_COUNT: usize = (ASCII_LAST - ASCII_FIRST + 1) as usize; // 95

/// Pre-rasterized fixed-width glyph atlas.
///
/// All ASCII printable characters (32-126) are rasterized into cells of
/// uniform width and height. Each pixel is stored as a coverage byte (0-255)
/// suitable for anti-aliased blending.
pub struct GlyphAtlas {
    cell_w: u16,
    cell_h: u16,
    /// Flat coverage data: 95 glyphs × cell_w × cell_h bytes.
    data: Vec<u8>,
    /// Replacement glyph for non-ASCII codepoints.
    replacement: Vec<u8>,
}

impl GlyphAtlas {
    /// Create a new atlas by pre-rasterizing all ASCII glyphs at `size_px`.
    pub fn new(font_data: &[u8], size_px: u16) -> Option<Self> {
        let renderer = FontRenderer::new(font_data)?;
        let upem = renderer.font.units_per_em() as f32;
        if upem == 0.0 {
            return None;
        }
        let scale = size_px as f32 / upem;
        let hhea = renderer.font.hhea();
        let ascender = libm::ceilf(hhea.ascender as f32 * scale) as i32;
        let descender = libm::floorf(hhea.descender as f32 * scale) as i32;
        let line_gap = libm::roundf(hhea.line_gap as f32 * scale) as i32;
        // Cell height includes ascender, descender, and half of line_gap
        // for inter-line padding.
        let cell_h = (ascender - descender + line_gap / 2) as u16;

        // Cell width = max ceil'd advance across all ASCII printable chars.
        let mut max_advance: u16 = 0;
        for cp in ASCII_FIRST..=ASCII_LAST {
            if let Some(gid) = renderer.font.glyph_index(cp) {
                if let Some(hm) = renderer.font.h_metrics(gid) {
                    let adv = libm::ceilf(hm.advance_width as f32 * scale) as u16;
                    if adv > max_advance {
                        max_advance = adv;
                    }
                }
            }
        }
        if max_advance == 0 || cell_h == 0 {
            return None;
        }
        let cell_w = max_advance;

        let stride = cell_w as usize * cell_h as usize;
        let mut data = vec![0u8; ASCII_COUNT * stride];

        for cp in ASCII_FIRST..=ASCII_LAST {
            let idx = (cp - ASCII_FIRST) as usize;
            let cell = &mut data[idx * stride..(idx + 1) * stride];

            if let Some(rg) = renderer.rasterize_glyph(cp, size_px, scale, ascender) {
                // Center glyph horizontally in cell.
                let glyph_advance = rg.advance as i32;
                let x_center = (cell_w as i32 - glyph_advance) / 2;
                let gx_start = x_center + rg.bearing_x as i32;
                let gy_start = ascender - rg.bearing_y as i32;

                for gy in 0..rg.height as usize {
                    for gx in 0..rg.width as usize {
                        let dx = gx_start + gx as i32;
                        let dy = gy_start + gy as i32;
                        if dx >= 0
                            && (dx as usize) < cell_w as usize
                            && dy >= 0
                            && (dy as usize) < cell_h as usize
                        {
                            let src = gy * rg.width as usize + gx;
                            let dst = dy as usize * cell_w as usize + dx as usize;
                            if src < rg.coverage.len() {
                                cell[dst] = rg.coverage[src];
                            }
                        }
                    }
                }
            }
        }

        // Replacement glyph: filled diamond.
        let mut replacement = vec![0u8; stride];
        let mx = cell_w as usize / 2;
        let my = cell_h as usize / 2;
        let rx = (cell_w as usize / 3).max(2);
        let ry = (cell_h as usize / 3).max(2);
        for y in 0..cell_h as usize {
            for x in 0..cell_w as usize {
                let dx = if x >= mx { x - mx } else { mx - x };
                let dy = if y >= my { y - my } else { my - y };
                if dx * ry + dy * rx <= rx * ry {
                    replacement[y * cell_w as usize + x] = 200;
                }
            }
        }

        Some(Self {
            cell_w,
            cell_h,
            data,
            replacement,
        })
    }

    #[inline]
    pub fn cell_width(&self) -> i32 {
        self.cell_w as i32
    }

    #[inline]
    pub fn cell_height(&self) -> i32 {
        self.cell_h as i32
    }

    /// Get coverage data for a codepoint (cell_w × cell_h bytes).
    #[inline]
    pub fn get_coverage(&self, codepoint: u32) -> &[u8] {
        if codepoint >= ASCII_FIRST && codepoint <= ASCII_LAST {
            let idx = (codepoint - ASCII_FIRST) as usize;
            let stride = self.cell_w as usize * self.cell_h as usize;
            &self.data[idx * stride..(idx + 1) * stride]
        } else {
            &self.replacement
        }
    }

    // -----------------------------------------------------------------------
    // Drawing helpers (Canvas-based)
    // -----------------------------------------------------------------------

    /// Draw a single character at (x, y). Uses bg for uncovered pixels when
    /// `bg.0 != 0`; otherwise blends onto the existing framebuffer content.
    pub fn draw_char<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        cp: u32,
        fg: Color32,
        bg: Color32,
    ) -> Option<DamageRect> {
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        let coverage = self.get_coverage(cp);
        let has_bg = bg.0 != 0;
        let fmt = target.pixel_format();
        let fg_px = fmt.encode(fg);
        let bg_px = fmt.encode(bg);

        let buf_w = target.width() as i32;
        let buf_h = target.height() as i32;

        for row in 0..ch {
            let py = y + row;
            if py < 0 || py >= buf_h {
                continue;
            }
            for col in 0..cw {
                let px = x + col;
                if px < 0 || px >= buf_w {
                    continue;
                }
                let cov = coverage[(row * cw + col) as usize];
                if cov == 0 {
                    if has_bg {
                        target.put_pixel(px, py, bg_px);
                    }
                } else if cov == 255 {
                    target.put_pixel(px, py, fg_px);
                } else if has_bg {
                    let blended = blend_color32(cov, fg, bg);
                    target.put_pixel(px, py, fmt.encode(blended));
                } else {
                    slopos_gfx::blend::put_pixel_coverage(target, px, py, fg, cov);
                }
            }
        }

        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + cw - 1).min(buf_w - 1);
        let y1 = (y + ch - 1).min(buf_h - 1);
        if x0 <= x1 && y0 <= y1 {
            let d = DamageRect { x0, y0, x1, y1 };
            target.report_damage(d);
            Some(d)
        } else {
            None
        }
    }

    /// Draw a null-terminated byte string.
    pub fn draw_bytes<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &[u8],
        fg: Color32,
        bg: Color32,
    ) -> Option<DamageRect> {
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        let w = target.width() as i32;
        let h = target.height() as i32;
        let mut cx = x;
        let mut cy = y;
        let mut damage: Option<DamageRect> = None;

        for &byte in text {
            match byte {
                0 => break,
                b'\n' => {
                    cx = x;
                    cy += ch;
                }
                b'\r' => cx = x,
                b'\t' => {
                    let tab = 4 * cw;
                    cx = ((cx - x + tab) / tab) * tab + x;
                }
                _ => {
                    if let Some(d) = self.draw_char(target, cx, cy, byte as u32, fg, bg) {
                        damage = Some(match damage {
                            Some(prev) => prev.union(&d),
                            None => d,
                        });
                    }
                    cx += cw;
                    if cx + cw > w {
                        cx = x;
                        cy += ch;
                    }
                }
            }
            if cy >= h {
                break;
            }
        }
        if let Some(d) = damage {
            target.report_damage(d);
        }
        damage
    }

    /// Draw a UTF-8 string.
    #[inline]
    pub fn draw_str<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        fg: Color32,
        bg: Color32,
    ) -> Option<DamageRect> {
        self.draw_bytes(target, x, y, text.as_bytes(), fg, bg)
    }

    /// Draw a single character, clipped to a rectangle.
    pub fn draw_char_clipped<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        cp: u32,
        fg: Color32,
        bg: Color32,
        clip: &DamageRect,
    ) {
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        if x > clip.x1 || y > clip.y1 || x + cw - 1 < clip.x0 || y + ch - 1 < clip.y0 {
            return;
        }

        let coverage = self.get_coverage(cp);
        let has_bg = bg.0 != 0;
        let fmt = target.pixel_format();
        let fg_px = fmt.encode(fg);
        let bg_px = fmt.encode(bg);

        for row in 0..ch {
            let py = y + row;
            if py < clip.y0 || py > clip.y1 {
                continue;
            }
            for col in 0..cw {
                let px = x + col;
                if px < clip.x0 || px > clip.x1 {
                    continue;
                }
                let cov = coverage[(row * cw + col) as usize];
                if cov == 0 {
                    if has_bg {
                        target.put_pixel(px, py, bg_px);
                    }
                } else if cov == 255 {
                    target.put_pixel(px, py, fg_px);
                } else if has_bg {
                    let blended = blend_color32(cov, fg, bg);
                    target.put_pixel(px, py, fmt.encode(blended));
                } else {
                    slopos_gfx::blend::put_pixel_coverage(target, px, py, fg, cov);
                }
            }
        }
    }

    /// Draw a UTF-8 string, clipped.
    pub fn draw_str_clipped<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        fg: Color32,
        bg: Color32,
        clip: &DamageRect,
    ) {
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        if y + ch - 1 < clip.y0 || y > clip.y1 {
            return;
        }
        let mut cx = x;
        for &byte in text.as_bytes() {
            if byte == 0 {
                break;
            }
            if cx > clip.x1 {
                break;
            }
            if cx + cw - 1 >= clip.x0 {
                self.draw_char_clipped(target, cx, y, byte as u32, fg, bg, clip);
            }
            cx += cw;
        }
    }

    /// Measure width of a null-terminated byte string.
    pub fn bytes_width(&self, text: &[u8]) -> i32 {
        let cw = self.cell_w as i32;
        let mut width = 0i32;
        for &ch in text {
            match ch {
                0 | b'\n' => break,
                b'\t' => {
                    let tab = 4 * cw;
                    width = ((width + tab - 1) / tab) * tab;
                }
                _ => width += cw,
            }
        }
        width
    }

    /// Measure width of a UTF-8 string.
    #[inline]
    pub fn str_width(&self, text: &str) -> i32 {
        self.bytes_width(text.as_bytes())
    }

    /// Count lines in a null-terminated byte string.
    pub fn bytes_lines(&self, text: &[u8]) -> i32 {
        let mut lines = 1i32;
        for &ch in text {
            if ch == 0 {
                break;
            }
            if ch == b'\n' {
                lines += 1;
            }
        }
        lines
    }
}

// ---------------------------------------------------------------------------
// Blending helpers
// ---------------------------------------------------------------------------

/// Blend fg and bg Color32 values by coverage (0-255).
#[inline]
fn blend_color32(cov: u8, fg: Color32, bg: Color32) -> Color32 {
    let a = cov as u32;
    let inv = 255 - a;
    let r = (fg.red() as u32 * a + bg.red() as u32 * inv + 128) / 255;
    let g = (fg.green() as u32 * a + bg.green() as u32 * inv + 128) / 255;
    let b = (fg.blue() as u32 * a + bg.blue() as u32 * inv + 128) / 255;
    let al = (fg.alpha() as u32 * a + bg.alpha() as u32 * inv + 128) / 255;
    Color32::new(r as u8, g as u8, b as u8, al as u8)
}

/// Blend fg and bg raw u32 (0x00RRGGBB) values by coverage.
/// Used by the vconsole which operates on raw pixel values.
#[inline]
pub fn blend_coverage_u32(cov: u8, fg: u32, bg: u32) -> u32 {
    if cov == 255 {
        return fg;
    }
    if cov == 0 {
        return bg;
    }
    let a = cov as u32;
    let inv = 255 - a;
    let r = (((fg >> 16) & 0xFF) * a + ((bg >> 16) & 0xFF) * inv + 128) / 255;
    let g = (((fg >> 8) & 0xFF) * a + ((bg >> 8) & 0xFF) * inv + 128) / 255;
    let b = ((fg & 0xFF) * a + (bg & 0xFF) * inv + 128) / 255;
    (r << 16) | (g << 8) | b
}

// ---------------------------------------------------------------------------
// Global atlas (kernel use)
// ---------------------------------------------------------------------------

static GLOBAL_ATLAS: AtomicPtr<GlyphAtlas> = AtomicPtr::new(core::ptr::null_mut());

/// Initialize the global glyph atlas. Call once during boot after the
/// allocator is ready.
pub fn init_global(font_data: &[u8], size_px: u16) -> bool {
    if let Some(atlas) = GlyphAtlas::new(font_data, size_px) {
        let ptr = Box::into_raw(Box::new(atlas));
        GLOBAL_ATLAS.store(ptr, Ordering::Release);
        true
    } else {
        false
    }
}

/// Get a reference to the global atlas (`None` before `init_global`).
pub fn global() -> Option<&'static GlyphAtlas> {
    let ptr = GLOBAL_ATLAS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // SAFETY: ptr was created via Box::into_raw during init_global
        // and is never freed or mutated after store.
        unsafe { Some(&*ptr) }
    }
}
