//! Drawing primitives for userland graphics
//!
//! All functions are 100% safe Rust - no unsafe blocks.
//! Drawing operations handle bounds checking and damage tracking automatically.

use super::DrawBuffer;

/// Fill a rectangle with a solid color
pub fn fill_rect(buf: &mut DrawBuffer, x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }

    let width = buf.width() as i32;
    let height = buf.height() as i32;

    // Clip to buffer bounds
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w - 1).min(width - 1);
    let y1 = (y + h - 1).min(height - 1);

    if x0 > x1 || y0 > y1 {
        return;
    }

    let pixel_format = super::PixelFormat::from_bpp(buf.bytes_pp() * 8);
    let converted = pixel_format.convert_color(color);
    let bytes_pp = buf.bytes_pp() as usize;
    let pitch = buf.pitch();
    let span_w = (x1 - x0 + 1) as usize;
    let data = buf.data_mut();

    for row in y0..=y1 {
        let row_off = (row as usize) * pitch + (x0 as usize) * bytes_pp;
        match bytes_pp {
            4 => {
                let row_slice = &mut data[row_off..row_off + span_w * 4];
                if converted == 0 {
                    row_slice.fill(0);
                } else {
                    let bytes = converted.to_le_bytes();
                    for chunk in row_slice.chunks_exact_mut(4) {
                        chunk.copy_from_slice(&bytes);
                    }
                }
            }
            3 => {
                let bytes = converted.to_le_bytes();
                for col in 0..span_w {
                    let off = row_off + col * 3;
                    if off + 3 <= data.len() {
                        data[off] = bytes[0];
                        data[off + 1] = bytes[1];
                        data[off + 2] = bytes[2];
                    }
                }
            }
            _ => {}
        }
    }

    buf.add_damage(x0, y0, x1, y1);
}

/// Draw a line using Bresenham's algorithm
pub fn draw_line(buf: &mut DrawBuffer, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let width = buf.width() as i32;
    let height = buf.height() as i32;

    let pixel_format = super::PixelFormat::from_bpp(buf.bytes_pp() * 8);
    let converted = pixel_format.convert_color(color);
    let bytes_pp = buf.bytes_pp();
    let pitch = buf.pitch();
    let data = buf.data_mut();

    let mut x = x0;
    let mut y = y0;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        // Draw pixel if within bounds
        if x >= 0 && x < width && y >= 0 && y < height {
            let offset = (y as usize) * pitch + (x as usize) * (bytes_pp as usize);
            write_pixel(data, offset, bytes_pp, converted);
        }

        if x == x1 && y == y1 {
            break;
        }

        let e2 = 2 * err;
        if e2 >= dy {
            if x == x1 {
                break;
            }
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            if y == y1 {
                break;
            }
            err += dx;
            y += sy;
        }
    }

    // Add damage for the line's bounding box
    let min_x = x0.min(x1).max(0);
    let min_y = y0.min(y1).max(0);
    let max_x = x0.max(x1).min(width - 1);
    let max_y = y0.max(y1).min(height - 1);
    buf.add_damage(min_x, min_y, max_x, max_y);
}

/// Draw a circle outline using the midpoint algorithm
pub fn draw_circle(buf: &mut DrawBuffer, cx: i32, cy: i32, radius: i32, color: u32) {
    if radius < 0 {
        return;
    }

    let width = buf.width() as i32;
    let height = buf.height() as i32;

    let pixel_format = super::PixelFormat::from_bpp(buf.bytes_pp() * 8);
    let converted = pixel_format.convert_color(color);
    let bytes_pp = buf.bytes_pp();
    let pitch = buf.pitch();
    let data = buf.data_mut();

    let mut x = radius;
    let mut y = 0;
    let mut err = 0;

    while x >= y {
        // Draw 8 octants
        let points = [
            (cx + x, cy + y),
            (cx + y, cy + x),
            (cx - y, cy + x),
            (cx - x, cy + y),
            (cx - x, cy - y),
            (cx - y, cy - x),
            (cx + y, cy - x),
            (cx + x, cy - y),
        ];

        for (px, py) in points {
            if px >= 0 && px < width && py >= 0 && py < height {
                let offset = (py as usize) * pitch + (px as usize) * (bytes_pp as usize);
                write_pixel(data, offset, bytes_pp, converted);
            }
        }

        y += 1;
        err += 1 + 2 * y;
        if 2 * (err - x) + 1 > 0 {
            x -= 1;
            err += 1 - 2 * x;
        }
    }

    // Add damage for the circle's bounding box
    let min_x = (cx - radius).max(0);
    let min_y = (cy - radius).max(0);
    let max_x = (cx + radius).min(width - 1);
    let max_y = (cy + radius).min(height - 1);
    buf.add_damage(min_x, min_y, max_x, max_y);
}

/// Draw a filled circle
pub fn draw_circle_filled(buf: &mut DrawBuffer, cx: i32, cy: i32, radius: i32, color: u32) {
    if radius < 0 {
        return;
    }

    let width = buf.width() as i32;
    let height = buf.height() as i32;

    let pixel_format = super::PixelFormat::from_bpp(buf.bytes_pp() * 8);
    let converted = pixel_format.convert_color(color);
    let bytes_pp = buf.bytes_pp();
    let pitch = buf.pitch();
    let data = buf.data_mut();

    let mut x = radius;
    let mut y = 0;
    let mut err = 0;

    while x >= y {
        // Draw horizontal lines to fill the circle
        draw_hline(data, pitch, bytes_pp, width, height, cx - x, cx + x, cy + y, converted);
        draw_hline(data, pitch, bytes_pp, width, height, cx - x, cx + x, cy - y, converted);
        draw_hline(data, pitch, bytes_pp, width, height, cx - y, cx + y, cy + x, converted);
        draw_hline(data, pitch, bytes_pp, width, height, cx - y, cx + y, cy - x, converted);

        y += 1;
        err += 1 + 2 * y;
        if 2 * (err - x) + 1 > 0 {
            x -= 1;
            err += 1 - 2 * x;
        }
    }

    // Add damage for the circle's bounding box
    let min_x = (cx - radius).max(0);
    let min_y = (cy - radius).max(0);
    let max_x = (cx + radius).min(width - 1);
    let max_y = (cy + radius).min(height - 1);
    buf.add_damage(min_x, min_y, max_x, max_y);
}

/// Draw a rectangle outline
pub fn draw_rect(buf: &mut DrawBuffer, x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }

    // Draw four edges
    draw_line(buf, x, y, x + w - 1, y, color); // Top
    draw_line(buf, x, y + h - 1, x + w - 1, y + h - 1, color); // Bottom
    draw_line(buf, x, y, x, y + h - 1, color); // Left
    draw_line(buf, x + w - 1, y, x + w - 1, y + h - 1, color); // Right
}

/// Blit (copy) a region within the buffer
pub fn blit(
    buf: &mut DrawBuffer,
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) {
    if width <= 0 || height <= 0 {
        return;
    }

    let buf_width = buf.width() as i32;
    let buf_height = buf.height() as i32;
    let bytes_pp = buf.bytes_pp() as usize;
    let pitch = buf.pitch();

    // Clip source region
    let src_x0 = src_x.max(0);
    let src_y0 = src_y.max(0);
    let src_x1 = (src_x + width - 1).min(buf_width - 1);
    let src_y1 = (src_y + height - 1).min(buf_height - 1);

    if src_x0 > src_x1 || src_y0 > src_y1 {
        return;
    }

    let actual_width = (src_x1 - src_x0 + 1) as usize;
    let actual_height = (src_y1 - src_y0 + 1) as usize;

    // Clip destination
    let dst_x0 = dst_x.max(0);
    let dst_y0 = dst_y.max(0);
    let dst_x1 = (dst_x + actual_width as i32 - 1).min(buf_width - 1);
    let dst_y1 = (dst_y + actual_height as i32 - 1).min(buf_height - 1);

    if dst_x0 > dst_x1 || dst_y0 > dst_y1 {
        return;
    }

    let copy_width = ((dst_x1 - dst_x0 + 1) as usize).min(actual_width);
    let copy_height = ((dst_y1 - dst_y0 + 1) as usize).min(actual_height);
    let row_bytes = copy_width * bytes_pp;

    let data = buf.data_mut();

    // Handle overlapping regions by copying in correct order
    if dst_y0 < src_y0 || (dst_y0 == src_y0 && dst_x0 < src_x0) {
        // Copy top-to-bottom, left-to-right
        for row in 0..copy_height {
            let src_off = ((src_y0 as usize + row) * pitch) + (src_x0 as usize * bytes_pp);
            let dst_off = ((dst_y0 as usize + row) * pitch) + (dst_x0 as usize * bytes_pp);
            data.copy_within(src_off..src_off + row_bytes, dst_off);
        }
    } else {
        // Copy bottom-to-top, right-to-left
        for row in (0..copy_height).rev() {
            let src_off = ((src_y0 as usize + row) * pitch) + (src_x0 as usize * bytes_pp);
            let dst_off = ((dst_y0 as usize + row) * pitch) + (dst_x0 as usize * bytes_pp);
            data.copy_within(src_off..src_off + row_bytes, dst_off);
        }
    }

    buf.add_damage(dst_x0, dst_y0, dst_x1, dst_y1);
}

/// Scroll the buffer contents up by a number of pixels
pub fn scroll_up(buf: &mut DrawBuffer, pixels: i32, fill_color: u32) {
    if pixels <= 0 {
        return;
    }

    let height = buf.height() as i32;
    let width = buf.width() as i32;

    if pixels >= height {
        // Scroll entire buffer - just clear
        buf.clear(fill_color);
        return;
    }

    // Copy from bottom to top
    blit(buf, 0, pixels, 0, 0, width, height - pixels);

    // Fill the bottom area with the fill color
    fill_rect(buf, 0, height - pixels, width, pixels, fill_color);
}

/// Scroll the buffer contents down by a number of pixels
pub fn scroll_down(buf: &mut DrawBuffer, pixels: i32, fill_color: u32) {
    if pixels <= 0 {
        return;
    }

    let height = buf.height() as i32;
    let width = buf.width() as i32;

    if pixels >= height {
        buf.clear(fill_color);
        return;
    }

    // Copy from top to bottom
    blit(buf, 0, 0, 0, pixels, width, height - pixels);

    // Fill the top area with the fill color
    fill_rect(buf, 0, 0, width, pixels, fill_color);
}

// =============================================================================
// Internal helper functions
// =============================================================================

/// Write a pixel to a data slice at the given offset
#[inline]
fn write_pixel(data: &mut [u8], offset: usize, bytes_pp: u8, color: u32) {
    match bytes_pp {
        4 => {
            if offset + 4 <= data.len() {
                let bytes = color.to_le_bytes();
                data[offset..offset + 4].copy_from_slice(&bytes);
            }
        }
        3 => {
            if offset + 3 <= data.len() {
                let bytes = color.to_le_bytes();
                data[offset] = bytes[0];
                data[offset + 1] = bytes[1];
                data[offset + 2] = bytes[2];
            }
        }
        _ => {}
    }
}

/// Draw a horizontal line
#[inline]
fn draw_hline(
    data: &mut [u8],
    pitch: usize,
    bytes_pp: u8,
    width: i32,
    height: i32,
    x0: i32,
    x1: i32,
    y: i32,
    color: u32,
) {
    if y < 0 || y >= height {
        return;
    }
    let x0 = x0.max(0);
    let x1 = x1.min(width - 1);
    if x0 > x1 {
        return;
    }

    let row_off = (y as usize) * pitch;
    for x in x0..=x1 {
        let offset = row_off + (x as usize) * (bytes_pp as usize);
        write_pixel(data, offset, bytes_pp, color);
    }
}
