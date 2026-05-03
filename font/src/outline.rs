//! Outline processing: convert TTF glyph outlines to rasterizer-ready edges.
//!
//! Handles quadratic Bézier curve flattening, scaling from font units to
//! pixels, and implied on-curve point insertion.

use slopos_ostd::KVec;

use crate::ttf_parser::{GlyphOutline, OutlinePoint};

/// A line segment in pixel coordinates used by the rasterizer.
#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// Convert a glyph outline from font units to pixel-space edges.
///
/// The outline is:
/// 1. Scaled from font units to the target pixel size
/// 2. Y-axis flipped (TTF has Y-up, screen has Y-down)
/// 3. Quadratic Bézier curves flattened into line segments
/// 4. Translated so the glyph's bounding box starts near (0, 0)
///
/// Returns a list of edges suitable for the scan-line rasterizer.
pub fn outline_to_edges(outline: &GlyphOutline, scale: f32, y_offset: f32) -> KVec<Edge> {
    let mut edges: KVec<Edge> = KVec::new();

    for contour in &outline.contours {
        if contour.points.is_empty() {
            continue;
        }

        // Insert implied on-curve points between consecutive off-curve points
        let expanded = expand_implied_points(&contour.points);
        if expanded.len() < 2 {
            continue;
        }

        let mut i = 0;
        let n = expanded.len();

        while i < n {
            let p0 = &expanded[i];
            let p1 = &expanded[(i + 1) % n];

            if p1.on_curve {
                // Line segment
                let x0 = p0.x as f32 * scale;
                let y0 = y_offset - p0.y as f32 * scale;
                let x1 = p1.x as f32 * scale;
                let y1 = y_offset - p1.y as f32 * scale;

                if (y0 - y1).abs() > 0.001 {
                    edges.push(Edge { x0, y0, x1, y1 }).expect("edges: alloc");
                }
                i += 1;
            } else {
                // Quadratic Bézier: p0 (on) -> p1 (off) -> p2 (on)
                let p2 = &expanded[(i + 2) % n];

                flatten_quadratic_bezier(
                    p0.x as f32 * scale,
                    y_offset - p0.y as f32 * scale,
                    p1.x as f32 * scale,
                    y_offset - p1.y as f32 * scale,
                    p2.x as f32 * scale,
                    y_offset - p2.y as f32 * scale,
                    &mut edges,
                );
                i += 2;
            }
        }
    }

    edges
}

/// Insert implied on-curve points between consecutive off-curve points.
///
/// In TrueType, when two consecutive points are both off-curve, there is an
/// implied on-curve point at their midpoint.
fn expand_implied_points(points: &[OutlinePoint]) -> KVec<OutlinePoint> {
    let n = points.len();
    if n == 0 {
        return KVec::new();
    }

    let mut result: KVec<OutlinePoint> = KVec::with_capacity(n * 2).expect("expand: alloc");

    for i in 0..n {
        let curr = points[i];
        let next = points[(i + 1) % n];

        result.push(curr).expect("expand: alloc");

        if !curr.on_curve && !next.on_curve {
            // Insert implied on-curve midpoint
            result
                .push(OutlinePoint {
                    x: ((curr.x as i32 + next.x as i32) / 2) as i16,
                    y: ((curr.y as i32 + next.y as i32) / 2) as i16,
                    on_curve: true,
                })
                .expect("expand: alloc");
        }
    }

    // Ensure the contour starts with an on-curve point
    if !result.is_empty() && !result[0].on_curve {
        // Find the first on-curve point and rotate
        if let Some(first_on) = result.iter().position(|p| p.on_curve) {
            result.as_mut_slice().rotate_left(first_on);
        }
    }

    result
}

/// Flatten a quadratic Bézier curve into line segments.
///
/// Uses recursive subdivision until segments are approximately flat.
fn flatten_quadratic_bezier(
    x0: f32,
    y0: f32,
    cx: f32,
    cy: f32,
    x1: f32,
    y1: f32,
    edges: &mut KVec<Edge>,
) {
    // Flatness test: distance of control point from line p0-p1
    let mx = (x0 + x1) * 0.5;
    let my = (y0 + y1) * 0.5;
    let dx = cx - mx;
    let dy = cy - my;
    let flatness = dx * dx + dy * dy;

    if flatness < 0.25 {
        // Flat enough — emit a single line segment
        if (y0 - y1).abs() > 0.001 {
            edges.push(Edge { x0, y0, x1, y1 }).expect("edges: alloc");
        }
        return;
    }

    // Subdivide at t=0.5
    let qx0 = (x0 + cx) * 0.5;
    let qy0 = (y0 + cy) * 0.5;
    let qx1 = (cx + x1) * 0.5;
    let qy1 = (cy + y1) * 0.5;
    let mx = (qx0 + qx1) * 0.5;
    let my = (qy0 + qy1) * 0.5;

    flatten_quadratic_bezier(x0, y0, qx0, qy0, mx, my, edges);
    flatten_quadratic_bezier(mx, my, qx1, qy1, x1, y1, edges);
}
