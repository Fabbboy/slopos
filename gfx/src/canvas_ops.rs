use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

use crate::blend::put_pixel_coverage;

#[inline]
fn emit<T: Canvas>(target: &mut T, damage: Option<DamageRect>) -> Option<DamageRect> {
    if let Some(d) = damage {
        target.report_damage(d);
    }
    damage
}

pub fn line<T: Canvas>(
    target: &mut T,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Color32,
) -> Option<DamageRect> {
    let w = target.width() as i32;
    let h = target.height() as i32;

    if (x0 < 0 && x1 < 0) || (y0 < 0 && y1 < 0) || (x0 >= w && x1 >= w) || (y0 >= h && y1 >= h) {
        return None;
    }

    let px = target.pixel_format().encode(color);

    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut cx = x0;
    let mut cy = y0;

    let mut min_x = x0.min(x1);
    let mut min_y = y0.min(y1);
    let mut max_x = x0.max(x1);
    let mut max_y = y0.max(y1);

    loop {
        target.put_pixel(cx, cy, px);
        if cx == x1 && cy == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            cx += sx;
        }
        if e2 <= dx {
            err += dx;
            cy += sy;
        }
    }

    min_x = min_x.max(0);
    min_y = min_y.max(0);
    max_x = max_x.min(w - 1);
    max_y = max_y.min(h - 1);

    let damage = if min_x <= max_x && min_y <= max_y {
        Some(DamageRect {
            x0: min_x,
            y0: min_y,
            x1: max_x,
            y1: max_y,
        })
    } else {
        None
    };
    emit(target, damage)
}

pub fn rect<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color32,
) -> Option<DamageRect> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let px = target.pixel_format().encode(color);
    target.hline(x, x + w - 1, y, px);
    target.hline(x, x + w - 1, y + h - 1, px);
    target.vline(x, y, y + h - 1, px);
    target.vline(x + w - 1, y, y + h - 1, px);

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w - 1).min(buf_w - 1);
    let y1 = (y + h - 1).min(buf_h - 1);

    let damage = if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    };
    emit(target, damage)
}

pub fn fill_rect<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color32,
) -> Option<DamageRect> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let px = target.pixel_format().encode(color);
    target.fill_rect_encoded(x, y, w, h, px);

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w - 1).min(buf_w - 1);
    let y1 = (y + h - 1).min(buf_h - 1);

    let damage = if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    };
    emit(target, damage)
}

pub fn fill_rect_clipped<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color32,
    clip: &DamageRect,
) {
    let rx0 = x.max(clip.x0);
    let ry0 = y.max(clip.y0);
    let rx1 = (x + w - 1).min(clip.x1);
    let ry1 = (y + h - 1).min(clip.y1);
    if rx0 <= rx1 && ry0 <= ry1 {
        fill_rect(target, rx0, ry0, rx1 - rx0 + 1, ry1 - ry0 + 1, color);
    }
}

pub fn circle<T: Canvas>(
    target: &mut T,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Color32,
) -> Option<DamageRect> {
    if radius <= 0 {
        return None;
    }
    let px = target.pixel_format().encode(color);

    let mut x = 0i32;
    let mut y = radius;
    let mut d = 1 - radius;

    while x <= y {
        target.put_pixel(cx + x, cy + y, px);
        target.put_pixel(cx - x, cy + y, px);
        target.put_pixel(cx + x, cy - y, px);
        target.put_pixel(cx - x, cy - y, px);
        target.put_pixel(cx + y, cy + x, px);
        target.put_pixel(cx - y, cy + x, px);
        target.put_pixel(cx + y, cy - x, px);
        target.put_pixel(cx - y, cy - x, px);

        x += 1;
        if d < 0 {
            d += 2 * x + 1;
        } else {
            y -= 1;
            d += 2 * (x - y) + 1;
        }
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = (cx - radius).max(0);
    let y0 = (cy - radius).max(0);
    let x1 = (cx + radius).min(buf_w - 1);
    let y1 = (cy + radius).min(buf_h - 1);

    let damage = if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    };
    emit(target, damage)
}

pub fn circle_filled<T: Canvas>(
    target: &mut T,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Color32,
) -> Option<DamageRect> {
    if radius <= 0 {
        return None;
    }
    let px = target.pixel_format().encode(color);

    let mut x = 0i32;
    let mut y = radius;
    let mut d = 1 - radius;

    target.hline(cx - radius, cx + radius, cy, px);

    while x < y {
        x += 1;
        if d < 0 {
            d += 2 * x + 1;
        } else {
            target.hline(cx - x + 1, cx + x - 1, cy + y, px);
            target.hline(cx - x + 1, cx + x - 1, cy - y, px);
            y -= 1;
            d += 2 * (x - y) + 1;
        }

        target.hline(cx - y, cx + y, cy + x, px);
        target.hline(cx - y, cx + y, cy - x, px);
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = (cx - radius).max(0);
    let y0 = (cy - radius).max(0);
    let x1 = (cx + radius).min(buf_w - 1);
    let y1 = (cy + radius).min(buf_h - 1);

    let damage = if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    };
    emit(target, damage)
}

pub fn triangle_filled<T: Canvas>(
    target: &mut T,
    mut x0: i32,
    mut y0: i32,
    mut x1: i32,
    mut y1: i32,
    mut x2: i32,
    mut y2: i32,
    color: Color32,
) -> Option<DamageRect> {
    let px = target.pixel_format().encode(color);

    if y0 > y1 {
        core::mem::swap(&mut y0, &mut y1);
        core::mem::swap(&mut x0, &mut x1);
    }
    if y1 > y2 {
        core::mem::swap(&mut y1, &mut y2);
        core::mem::swap(&mut x1, &mut x2);
    }
    if y0 > y1 {
        core::mem::swap(&mut y0, &mut y1);
        core::mem::swap(&mut x0, &mut x1);
    }

    let total_height = y2 - y0;
    if total_height == 0 {
        return None;
    }

    for y in y0..=y2 {
        let second_half = y > y1 || y1 == y0;
        let segment_height = if second_half { y2 - y1 } else { y1 - y0 };
        if segment_height == 0 {
            continue;
        }

        let dy = y - if second_half { y1 } else { y0 };
        let alpha = ((y - y0) as i64 * 65536) / total_height as i64;
        let beta = (dy as i64 * 65536) / segment_height as i64;

        let ax = x0 + (((x2 - x0) as i64 * alpha) >> 16) as i32;
        let bx = if second_half {
            x1 + (((x2 - x1) as i64 * beta) >> 16) as i32
        } else {
            x0 + (((x1 - x0) as i64 * beta) >> 16) as i32
        };

        let (xa, xb) = if ax < bx { (ax, bx) } else { (bx, ax) };
        target.hline(xa, xb, y, px);
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let min_x = x0.min(x1).min(x2).max(0);
    let min_y = y0.max(0);
    let max_x = x0.max(x1).max(x2).min(buf_w - 1);
    let max_y = y2.min(buf_h - 1);

    let damage = if min_x <= max_x && min_y <= max_y {
        Some(DamageRect {
            x0: min_x,
            y0: min_y,
            x1: max_x,
            y1: max_y,
        })
    } else {
        None
    };
    emit(target, damage)
}

// ---------------------------------------------------------------------------
// Anti-aliased drawing primitives (integer-only arithmetic)
// ---------------------------------------------------------------------------

/// Integer square root via Newton's method. Returns floor(sqrt(n)).
fn isqrt(n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Compute coverage for a circle boundary pixel using integer math.
///
/// Given (x, y) relative to the circle center and radius r, computes how
/// much of this pixel is "on the circle outline". Uses the distance error
/// from the ideal radius in 8.8 fixed-point space.
///
/// Returns 0-255 coverage value.
#[inline]
fn circle_coverage(x: i32, y: i32, r_x256: i32) -> u8 {
    // dist * 256 ≈ isqrt(dist_sq * 65536)
    let dist_sq = x * x + y * y;
    let dist_x256 = isqrt((dist_sq as u32) << 16) as i32;
    // err_x256 = dist*256 - r*256
    let err_x256 = dist_x256 - r_x256;
    // coverage = clamp(256 - err_x256, 0, 256) * 255 / 256
    let cov = (256 - err_x256).clamp(0, 256);
    ((cov * 255 + 128) >> 8) as u8
}

/// Compute inner-pixel coverage (for the pixel one step inside the circle).
#[inline]
fn circle_coverage_inner(x: i32, y: i32, r_x256: i32) -> u8 {
    let dist_sq = x * x + y * y;
    let dist_x256 = isqrt((dist_sq as u32) << 16) as i32;
    // err = r - dist (positive when inside)
    let err_x256 = r_x256 - dist_x256;
    let cov = (256 - err_x256).clamp(0, 256);
    ((cov * 255 + 128) >> 8) as u8
}

/// Should the midpoint circle stepper move y inward?
/// Uses integer comparison: (x+1)^2 + y^2 > (r + 0.5)^2
/// Scaled by 4 to avoid fractions: (2x+2)^2 + (2y)^2 > (2r+1)^2
#[inline]
fn circle_should_step_y(x: i32, y: i32, r: i32) -> bool {
    let a = 2 * (x + 1);
    let b = 2 * y;
    let c = 2 * r + 1;
    a * a + b * b > c * c
}

/// Xiaolin Wu's anti-aliased line algorithm.
///
/// Draws a smooth line from `(x0, y0)` to `(x1, y1)` by blending pixels
/// at fractional boundaries. Uses integer fixed-point arithmetic.
pub fn line_aa<T: Canvas>(
    target: &mut T,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Color32,
) -> Option<DamageRect> {
    let w = target.width() as i32;
    let h = target.height() as i32;

    if (x0 < 0 && x1 < 0) || (y0 < 0 && y1 < 0) || (x0 >= w && x1 >= w) || (y0 >= h && y1 >= h)
    {
        return None;
    }

    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();

    if dx == 0 && dy == 0 {
        put_pixel_coverage(target, x0, y0, color, 255);
        let damage = DamageRect {
            x0,
            y0,
            x1: x0,
            y1: y0,
        };
        return emit(target, Some(damage));
    }

    let steep = dy > dx;

    let (mut px0, mut py0, mut px1, mut py1) = if steep {
        (y0, x0, y1, x1)
    } else {
        (x0, y0, x1, y1)
    };

    if px0 > px1 {
        core::mem::swap(&mut px0, &mut px1);
        core::mem::swap(&mut py0, &mut py1);
    }

    let dx_f = px1 - px0;
    let dy_f = py1 - py0;

    // Gradient in 8.8 fixed point
    let gradient = if dx_f == 0 {
        256i32
    } else {
        (dy_f * 256) / dx_f
    };

    let plot = |t: &mut T, px: i32, py: i32, cov: u8| {
        if steep {
            put_pixel_coverage(t, py, px, color, cov);
        } else {
            put_pixel_coverage(t, px, py, color, cov);
        }
    };

    // First endpoint
    plot(target, px0, py0, 255);

    let mut intery = py0 * 256 + gradient;

    // Second endpoint
    plot(target, px1, py1, 255);

    // Main loop
    for x in (px0 + 1)..px1 {
        let y_int = intery / 256;
        let frac = (intery & 0xFF) as u8;

        plot(target, x, y_int, 255 - frac);
        plot(target, x, y_int + 1, frac);

        intery += gradient;
    }

    let min_x_aa = x0.min(x1).max(0);
    let min_y_aa = y0.min(y1).max(0);
    let max_x_aa = x0.max(x1).min(w - 1);
    let max_y_aa = (y0.max(y1) + 1).min(h - 1);

    let damage = if min_x_aa <= max_x_aa && min_y_aa <= max_y_aa {
        Some(DamageRect {
            x0: min_x_aa,
            y0: min_y_aa,
            x1: max_x_aa,
            y1: max_y_aa,
        })
    } else {
        None
    };
    emit(target, damage)
}

/// Anti-aliased circle outline using integer-only distance computation.
pub fn circle_aa<T: Canvas>(
    target: &mut T,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Color32,
) -> Option<DamageRect> {
    if radius <= 0 {
        return None;
    }

    let r_x256 = radius * 256;

    let mut x = 0i32;
    let mut y = radius;

    while x <= y {
        let cov = circle_coverage(x, y, r_x256);

        let pairs: [(i32, i32); 8] = [
            (cx + x, cy + y),
            (cx - x, cy + y),
            (cx + x, cy - y),
            (cx - x, cy - y),
            (cx + y, cy + x),
            (cx - y, cy + x),
            (cx + y, cy - x),
            (cx - y, cy - x),
        ];
        for &(px, py) in &pairs {
            put_pixel_coverage(target, px, py, color, cov);
        }

        if y > 0 {
            let cov_inner = circle_coverage_inner(x, y - 1, r_x256);
            if cov_inner > 0 {
                let inner_pairs: [(i32, i32); 8] = [
                    (cx + x, cy + y - 1),
                    (cx - x, cy + y - 1),
                    (cx + x, cy - y + 1),
                    (cx - x, cy - y + 1),
                    (cx + y - 1, cy + x),
                    (cx - y + 1, cy + x),
                    (cx + y - 1, cy - x),
                    (cx - y + 1, cy - x),
                ];
                for &(px, py) in &inner_pairs {
                    put_pixel_coverage(target, px, py, color, cov_inner);
                }
            }
        }

        x += 1;
        if circle_should_step_y(x - 1, y, radius) {
            y -= 1;
        }
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let dx0 = (cx - radius - 1).max(0);
    let dy0 = (cy - radius - 1).max(0);
    let dx1 = (cx + radius + 1).min(buf_w - 1);
    let dy1 = (cy + radius + 1).min(buf_h - 1);

    let damage = if dx0 <= dx1 && dy0 <= dy1 {
        Some(DamageRect {
            x0: dx0,
            y0: dy0,
            x1: dx1,
            y1: dy1,
        })
    } else {
        None
    };
    emit(target, damage)
}

/// Draw a rounded rectangle outline with anti-aliased corners (integer-only).
pub fn rounded_rect<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
    color: Color32,
) -> Option<DamageRect> {
    if w <= 0 || h <= 0 {
        return None;
    }

    let r = radius.min(w / 2).min(h / 2).max(0);

    if r == 0 {
        return rect(target, x, y, w, h, color);
    }

    let px = target.pixel_format().encode(color);

    // Straight edges (between corner arcs)
    target.hline(x + r, x + w - 1 - r, y, px);
    target.hline(x + r, x + w - 1 - r, y + h - 1, px);
    target.vline(x, y + r, y + h - 1 - r, px);
    target.vline(x + w - 1, y + r, y + h - 1 - r, px);

    // Corner arcs
    let corners = [
        (x + r, y + r, -1i32, -1i32),
        (x + w - 1 - r, y + r, 1, -1),
        (x + r, y + h - 1 - r, -1, 1),
        (x + w - 1 - r, y + h - 1 - r, 1, 1),
    ];

    let r_x256 = r * 256;

    for &(ccx, ccy, sx, sy) in &corners {
        let mut ci = 0i32;
        let mut cj = r;

        while ci <= cj {
            let cov = circle_coverage(ci, cj, r_x256);

            put_pixel_coverage(target, ccx + ci * sx, ccy + cj * sy, color, cov);
            if ci != cj {
                put_pixel_coverage(target, ccx + cj * sx, ccy + ci * sy, color, cov);
            }

            if cj > 0 {
                let cov_inner = circle_coverage_inner(ci, cj - 1, r_x256);
                if cov_inner > 0 {
                    put_pixel_coverage(
                        target,
                        ccx + ci * sx,
                        ccy + (cj - 1) * sy,
                        color,
                        cov_inner,
                    );
                    if ci != cj - 1 {
                        put_pixel_coverage(
                            target,
                            ccx + (cj - 1) * sx,
                            ccy + ci * sy,
                            color,
                            cov_inner,
                        );
                    }
                }
            }

            ci += 1;
            if circle_should_step_y(ci - 1, cj, r) {
                cj -= 1;
            }
        }
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let dx0 = x.max(0);
    let dy0 = y.max(0);
    let dx1 = (x + w - 1).min(buf_w - 1);
    let dy1 = (y + h - 1).min(buf_h - 1);

    let damage = if dx0 <= dx1 && dy0 <= dy1 {
        Some(DamageRect {
            x0: dx0,
            y0: dy0,
            x1: dx1,
            y1: dy1,
        })
    } else {
        None
    };
    emit(target, damage)
}

/// Draw a filled rounded rectangle with anti-aliased corners (integer-only).
pub fn rounded_rect_filled<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
    color: Color32,
) -> Option<DamageRect> {
    if w <= 0 || h <= 0 {
        return None;
    }

    let r = radius.min(w / 2).min(h / 2).max(0);
    let px = target.pixel_format().encode(color);

    if r == 0 {
        return fill_rect(target, x, y, w, h, color);
    }

    // Fill three rectangular regions (non-corner areas)
    target.fill_rect_encoded(x, y + r, w, h - 2 * r, px);
    target.fill_rect_encoded(x + r, y, w - 2 * r, r, px);
    target.fill_rect_encoded(x + r, y + h - r, w - 2 * r, r, px);

    // Fill corner regions with AA at the boundary
    let tl_cx = x + r;
    let tr_cx = x + w - 1 - r;
    let tl_cy = y + r;
    let bl_cy = y + h - 1 - r;
    let r_x256 = r * 256;

    let mut ci = 0i32;
    let mut cj = r;

    while ci <= cj {
        let cov = circle_coverage(ci, cj, r_x256);

        let row_top = tl_cy - cj;
        let row_bot = bl_cy + cj;

        // Fill interior of this row in both corners, then AA boundary pixel
        if ci > 0 {
            target.hline(tl_cx - ci + 1, tl_cx - 1, row_top, px);
            target.hline(tr_cx + 1, tr_cx + ci - 1, row_top, px);
            target.hline(tl_cx - ci + 1, tl_cx - 1, row_bot, px);
            target.hline(tr_cx + 1, tr_cx + ci - 1, row_bot, px);
        }
        put_pixel_coverage(target, tl_cx - ci, row_top, color, cov);
        put_pixel_coverage(target, tr_cx + ci, row_top, color, cov);
        put_pixel_coverage(target, tl_cx - ci, row_bot, color, cov);
        put_pixel_coverage(target, tr_cx + ci, row_bot, color, cov);

        // Swapped axis row
        if ci != cj {
            let row_top2 = tl_cy - ci;
            let row_bot2 = bl_cy + ci;

            if cj > 0 {
                target.hline(tl_cx - cj + 1, tl_cx - 1, row_top2, px);
                target.hline(tr_cx + 1, tr_cx + cj - 1, row_top2, px);
                target.hline(tl_cx - cj + 1, tl_cx - 1, row_bot2, px);
                target.hline(tr_cx + 1, tr_cx + cj - 1, row_bot2, px);
            }
            put_pixel_coverage(target, tl_cx - cj, row_top2, color, cov);
            put_pixel_coverage(target, tr_cx + cj, row_top2, color, cov);
            put_pixel_coverage(target, tl_cx - cj, row_bot2, color, cov);
            put_pixel_coverage(target, tr_cx + cj, row_bot2, color, cov);
        }

        ci += 1;
        if circle_should_step_y(ci - 1, cj, r) {
            cj -= 1;
        }
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let dx0 = x.max(0);
    let dy0 = y.max(0);
    let dx1 = (x + w - 1).min(buf_w - 1);
    let dy1 = (y + h - 1).min(buf_h - 1);

    let damage = if dx0 <= dx1 && dy0 <= dy1 {
        Some(DamageRect {
            x0: dx0,
            y0: dy0,
            x1: dx1,
            y1: dy1,
        })
    } else {
        None
    };
    emit(target, damage)
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use slopos_abi::draw::EncodedPixel;
    use slopos_abi::pixel::PixelFormat;

    struct TestCanvas {
        data: alloc::vec::Vec<u8>,
        width: u32,
        height: u32,
    }

    impl TestCanvas {
        fn new(width: u32, height: u32) -> Self {
            Self {
                data: alloc::vec![0u8; (width * height * 4) as usize],
                width,
                height,
            }
        }

        fn read_pixel(&self, x: u32, y: u32) -> Color32 {
            let off = (y * self.width + x) as usize * 4;
            let raw = u32::from_le_bytes([
                self.data[off], self.data[off + 1],
                self.data[off + 2], self.data[off + 3],
            ]);
            PixelFormat::Argb8888.decode(raw)
        }
    }

    impl Canvas for TestCanvas {
        fn width(&self) -> u32 { self.width }
        fn height(&self) -> u32 { self.height }
        fn pitch_bytes(&self) -> usize { self.width as usize * 4 }
        fn bytes_per_pixel(&self) -> u8 { 4 }
        fn pixel_format(&self) -> PixelFormat { PixelFormat::Argb8888 }

        fn write_encoded_at(&mut self, byte_offset: usize, pixel: EncodedPixel) {
            let bytes = pixel.to_u32().to_le_bytes();
            if byte_offset + 4 <= self.data.len() {
                self.data[byte_offset..byte_offset + 4].copy_from_slice(&bytes);
            }
        }

        fn read_encoded_at(&self, byte_offset: usize) -> u32 {
            if byte_offset + 4 <= self.data.len() {
                u32::from_le_bytes([
                    self.data[byte_offset], self.data[byte_offset + 1],
                    self.data[byte_offset + 2], self.data[byte_offset + 3],
                ])
            } else { 0 }
        }
    }

    #[test]
    fn line_draws_horizontal() {
        let mut c = TestCanvas::new(10, 10);
        line(&mut c, 0, 0, 9, 0, Color32::WHITE);
        for x in 0..10 {
            assert_eq!(c.read_pixel(x, 0), Color32::WHITE);
        }
        assert_eq!(c.read_pixel(0, 1).red(), 0);
    }

    #[test]
    fn fill_rect_fills_area() {
        let mut c = TestCanvas::new(20, 20);
        let red = Color32::rgb(255, 0, 0);
        fill_rect(&mut c, 5, 5, 3, 3, red);
        assert_eq!(c.read_pixel(5, 5), red);
        assert_eq!(c.read_pixel(7, 7), red);
        assert_ne!(c.read_pixel(4, 5), red);
        assert_ne!(c.read_pixel(8, 5), red);
    }

    #[test]
    fn line_aa_produces_output() {
        let mut c = TestCanvas::new(20, 20);
        let d = line_aa(&mut c, 0, 0, 19, 10, Color32::WHITE);
        assert!(d.is_some());
        assert!(c.read_pixel(0, 0).red() > 0);
    }

    #[test]
    fn circle_aa_outline_not_filled() {
        let mut c = TestCanvas::new(50, 50);
        circle_aa(&mut c, 25, 25, 10, Color32::WHITE);
        assert!(c.read_pixel(25, 15).red() > 0, "boundary should be drawn");
        assert_eq!(c.read_pixel(25, 25).red(), 0, "center should be empty");
    }

    #[test]
    fn rounded_rect_edge_and_interior() {
        let mut c = TestCanvas::new(60, 40);
        rounded_rect(&mut c, 5, 5, 50, 30, 8, Color32::WHITE);
        assert_eq!(c.read_pixel(30, 5), Color32::WHITE, "top edge");
        assert_eq!(c.read_pixel(30, 20).red(), 0, "interior empty");
    }

    #[test]
    fn rounded_rect_filled_interior() {
        let mut c = TestCanvas::new(60, 40);
        let color = Color32::rgb(100, 150, 200);
        rounded_rect_filled(&mut c, 5, 5, 50, 30, 8, color);
        assert_eq!(c.read_pixel(30, 20), color, "interior filled");
        assert_eq!(c.read_pixel(0, 0).red(), 0, "outside empty");
    }

    #[test]
    fn rounded_rect_zero_radius_equals_rect() {
        let mut c1 = TestCanvas::new(30, 30);
        let mut c2 = TestCanvas::new(30, 30);
        let color = Color32::rgb(50, 100, 150);
        rounded_rect(&mut c1, 5, 5, 20, 20, 0, color);
        rect(&mut c2, 5, 5, 20, 20, color);
        assert_eq!(c1.data, c2.data);
    }

    #[test]
    fn blend_put_pixel_on_canvas() {
        let mut c = TestCanvas::new(10, 10);
        fill_rect(&mut c, 0, 0, 10, 10, Color32::rgb(0, 0, 255));
        crate::blend::put_pixel_blended(&mut c, 5, 5, Color32::new(255, 0, 0, 128));
        let px = c.read_pixel(5, 5);
        assert!(px.red() > 50 && px.red() < 200, "r={}", px.red());
        assert!(px.blue() > 50, "b={}", px.blue());
    }

    #[test]
    fn fill_rect_blended_semitransparent() {
        let mut c = TestCanvas::new(10, 10);
        fill_rect(&mut c, 0, 0, 10, 10, Color32::WHITE);
        crate::blend::fill_rect_blended(&mut c, 2, 2, 6, 6, Color32::new(0, 0, 0, 128));
        let px = c.read_pixel(5, 5);
        assert!((px.red() as i32 - 127).abs() <= 5, "r={}", px.red());
        assert_eq!(c.read_pixel(0, 0), Color32::WHITE, "outside untouched");
    }
}
