//! Geometric drawing primitives
//!
//! All algorithms are implemented generically over `DrawTarget`.
//! This replaces duplicate code in video/src/graphics.rs and
//! userland/src/gfx/primitives.rs.

use crate::draw::DrawTarget;

/// Draw a line using Bresenham's algorithm
pub fn line<T: DrawTarget>(target: &mut T, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let raw = target.pixel_format().convert_color(color);
    let w = target.width() as i32;
    let h = target.height() as i32;

    // Early reject if entirely outside bounds
    if (x0 < 0 && x1 < 0) || (y0 < 0 && y1 < 0) || (x0 >= w && x1 >= w) || (y0 >= h && y1 >= h) {
        return;
    }

    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;

    loop {
        target.draw_pixel(x, y, raw);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Draw a rectangle outline
pub fn rect<T: DrawTarget>(target: &mut T, x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let raw = target.pixel_format().convert_color(color);
    target.draw_hline(x, x + w - 1, y, raw);
    target.draw_hline(x, x + w - 1, y + h - 1, raw);
    target.draw_vline(x, y, y + h - 1, raw);
    target.draw_vline(x + w - 1, y, y + h - 1, raw);
}

/// Fill a rectangle with a solid color
pub fn fill_rect<T: DrawTarget>(target: &mut T, x: i32, y: i32, w: i32, h: i32, color: u32) {
    let raw = target.pixel_format().convert_color(color);
    target.fill_rect(x, y, w, h, raw);
}

/// Draw a circle outline using the midpoint algorithm
pub fn circle<T: DrawTarget>(target: &mut T, cx: i32, cy: i32, radius: i32, color: u32) {
    if radius <= 0 {
        return;
    }
    let raw = target.pixel_format().convert_color(color);

    let mut x = 0i32;
    let mut y = radius;
    let mut d = 1 - radius;

    while x <= y {
        target.draw_pixel(cx + x, cy + y, raw);
        target.draw_pixel(cx - x, cy + y, raw);
        target.draw_pixel(cx + x, cy - y, raw);
        target.draw_pixel(cx - x, cy - y, raw);
        target.draw_pixel(cx + y, cy + x, raw);
        target.draw_pixel(cx - y, cy + x, raw);
        target.draw_pixel(cx + y, cy - x, raw);
        target.draw_pixel(cx - y, cy - x, raw);

        x += 1;
        if d < 0 {
            d += 2 * x + 1;
        } else {
            y -= 1;
            d += 2 * (x - y) + 1;
        }
    }
}

/// Draw a filled circle
pub fn circle_filled<T: DrawTarget>(target: &mut T, cx: i32, cy: i32, radius: i32, color: u32) {
    if radius <= 0 {
        return;
    }
    let raw = target.pixel_format().convert_color(color);

    let mut x = 0i32;
    let mut y = radius;
    let mut d = 1 - radius;

    target.draw_hline(cx - radius, cx + radius, cy, raw);

    while x < y {
        x += 1;
        if d < 0 {
            d += 2 * x + 1;
        } else {
            target.draw_hline(cx - x + 1, cx + x - 1, cy + y, raw);
            target.draw_hline(cx - x + 1, cx + x - 1, cy - y, raw);
            y -= 1;
            d += 2 * (x - y) + 1;
        }

        target.draw_hline(cx - y, cx + y, cy + x, raw);
        target.draw_hline(cx - y, cx + y, cy - x, raw);
    }
}

/// Draw a filled triangle using scanline algorithm
pub fn triangle_filled<T: DrawTarget>(
    target: &mut T,
    mut x0: i32,
    mut y0: i32,
    mut x1: i32,
    mut y1: i32,
    mut x2: i32,
    mut y2: i32,
    color: u32,
) {
    let raw = target.pixel_format().convert_color(color);

    // Sort vertices by y coordinate
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
        return;
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
        target.draw_hline(xa, xb, y, raw);
    }
}
