//! Scan-line coverage rasterizer for glyph outlines.
//!
//! Converts a list of edges (line segments) into a coverage bitmap using
//! the non-zero winding rule with 4× vertical supersampling.

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

/// Number of vertical sub-scanlines per pixel row for supersampling.
const SUPERSAMPLE: usize = 4;

/// Rasterize a list of edges into a coverage bitmap.
///
/// `width` and `height` are the bitmap dimensions in pixels.
/// Edges should already be in pixel coordinates (post-scaling).
pub fn rasterize(edges: &[Edge], width: usize, height: usize) -> Vec<u8> {
    if width == 0 || height == 0 || edges.is_empty() {
        return vec![0u8; width * height];
    }

    let sub_height = height * SUPERSAMPLE;
    let mut coverage = vec![0u8; width * height];

    // For each sub-scanline, compute winding at each pixel
    // We accumulate sub-pixel coverage and divide by SUPERSAMPLE at the end.

    // Pre-allocate a winding buffer for one scanline
    let mut winding = vec![0i32; width + 1];

    for sub_y in 0..sub_height {
        let y = sub_y as f32 / SUPERSAMPLE as f32 + 0.5 / SUPERSAMPLE as f32;

        // Clear winding buffer
        for w in winding.iter_mut() {
            *w = 0;
        }

        // For each edge, if it crosses this sub-scanline, compute the x intercept
        // and add to the winding buffer
        for edge in edges {
            let (mut ey0, mut ey1, mut ex0, mut ex1) = (edge.y0, edge.y1, edge.x0, edge.x1);

            // Determine winding direction
            let dir: i32;
            if ey0 < ey1 {
                dir = 1;
            } else {
                core::mem::swap(&mut ey0, &mut ey1);
                core::mem::swap(&mut ex0, &mut ex1);
                dir = -1;
            }

            // Check if scanline is in range
            if y < ey0 || y >= ey1 {
                continue;
            }

            // Compute x intercept
            let t = (y - ey0) / (ey1 - ey0);
            let x = ex0 + t * (ex1 - ex0);

            let xi = x as i32;
            if xi >= 0 && (xi as usize) < width {
                winding[xi as usize] += dir;
            }
        }

        // Accumulate winding number left-to-right and add to coverage
        let pixel_row = sub_y / SUPERSAMPLE;
        let mut wind = 0i32;
        for px in 0..width {
            wind += winding[px];
            if wind != 0 {
                // This pixel is inside the glyph for this sub-scanline
                let idx = pixel_row * width + px;
                // Each sub-scanline contributes 255/SUPERSAMPLE to the coverage
                // We'll accumulate as u16 then clamp
                let current = coverage[idx] as u16;
                let add = (255 / SUPERSAMPLE as u16).min(255 - current);
                coverage[idx] = (current + add) as u8;
            }
        }
    }

    coverage
}
