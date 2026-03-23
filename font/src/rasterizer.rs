//! Scan-line coverage rasterizer for glyph outlines.
//!
//! Computes per-pixel coverage using the non-zero winding rule with
//! analytical horizontal coverage (fractional x-intercepts) and 8×
//! vertical supersampling.  This matches the approach used by FreeType,
//! stb_truetype, and ab-glyph — boundary pixels get smooth sub-pixel
//! coverage instead of binary in/out.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use crate::outline::Edge;

/// A rasterized glyph's coverage bitmap and positioning metrics.
#[derive(Clone, Debug)]
pub struct RasterizedGlyph {
    /// Width of the coverage bitmap in pixels.
    pub width: u16,
    /// Height of the coverage bitmap in pixels.
    pub height: u16,
    /// Horizontal bearing (distance from origin to left edge).
    pub bearing_x: i16,
    /// Vertical bearing (distance from baseline to top edge).
    pub bearing_y: i16,
    /// Horizontal advance (distance to next glyph origin).
    pub advance: u16,
    /// Coverage values, row-major, `width * height` bytes.
    /// Each byte is 0 (transparent) to 255 (fully covered).
    pub coverage: Vec<u8>,
}

/// Number of vertical sub-scanlines per pixel row.
const SUPERSAMPLE: usize = 8;

/// Rasterize a list of edges into a coverage bitmap.
///
/// Uses analytical horizontal coverage (the fractional x-intercept
/// determines how much of the boundary pixel is covered) combined with
/// vertical supersampling for diagonal edges.
pub fn rasterize(edges: &[Edge], width: usize, height: usize) -> Vec<u8> {
    if width == 0 || height == 0 || edges.is_empty() {
        return vec![0u8; width * height];
    }

    let sub_height = height * SUPERSAMPLE;
    let inv_ss = 1.0f32 / SUPERSAMPLE as f32;

    // Per-pixel area accumulator (0.0 = uncovered, ±1.0 = fully covered).
    let mut area = vec![0.0f32; width * height];

    // Per-scanline winding delta buffer (reused each sub-scanline).
    let mut scanline_fill = vec![0.0f32; width + 1];

    for sub_y in 0..sub_height {
        // Sample at the centre of each sub-scanline.
        let y = (sub_y as f32 + 0.5) * inv_ss;
        let pixel_row = sub_y / SUPERSAMPLE;

        // Clear the fill-delta buffer.
        for d in scanline_fill.iter_mut() {
            *d = 0.0;
        }

        // Process each edge that crosses this sub-scanline.
        for edge in edges {
            let (mut ey0, mut ey1, mut ex0, mut ex1) = (edge.y0, edge.y1, edge.x0, edge.x1);

            let dir: f32;
            if ey0 < ey1 {
                dir = 1.0;
            } else {
                core::mem::swap(&mut ey0, &mut ey1);
                core::mem::swap(&mut ex0, &mut ex1);
                dir = -1.0;
            }

            if y < ey0 || y >= ey1 {
                continue;
            }

            // Exact x-intercept on this sub-scanline.
            let t = (y - ey0) / (ey1 - ey0);
            let x = ex0 + t * (ex1 - ex0);

            let xi = libm::floorf(x) as i32;
            let frac = x - libm::floorf(x); // 0..1 within pixel

            if xi >= 0 && (xi as usize) < width {
                // Boundary pixel: analytical partial coverage.
                // The edge enters at `frac` within the pixel, so the
                // fraction to the RIGHT of the edge is `(1 - frac)`.
                let row_off = pixel_row * width;
                area[row_off + xi as usize] += dir * (1.0 - frac) * inv_ss;

                // All pixels to the right are fully inside for this
                // sub-scanline — record a fill-delta.
                if (xi as usize + 1) <= width {
                    scanline_fill[xi as usize + 1] += dir;
                }
            } else if xi < 0 {
                // Edge is left of the bitmap — everything visible is
                // inside the glyph on this sub-scanline.
                scanline_fill[0] += dir;
            }
            // xi >= width: edge is past the right side, no visible effect.
        }

        // Accumulate the fill-delta left-to-right and add to each
        // pixel's area (one full sub-scanline contribution per pixel
        // that is inside the glyph).
        let row_off = pixel_row * width;
        let mut fill = 0.0f32;
        for px in 0..width {
            fill += scanline_fill[px];
            if fill != 0.0 {
                area[row_off + px] += fill * inv_ss;
            }
        }
    }

    // Convert accumulated area to 0-255 coverage.
    let mut coverage = vec![0u8; width * height];
    for (idx, &a) in area.iter().enumerate() {
        let cov = libm::fabsf(a);
        coverage[idx] = if cov >= 1.0 {
            255
        } else {
            (cov * 255.0 + 0.5) as u8
        };
    }

    coverage
}
