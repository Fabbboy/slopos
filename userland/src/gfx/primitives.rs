use super::DrawBuffer;
use slopos_abi::draw_primitives;

pub fn fill_rect(buf: &mut DrawBuffer, x: i32, y: i32, w: i32, h: i32, color: u32) {
    draw_primitives::fill_rect(buf, x, y, w, h, color);
    let x1 = x.max(0);
    let y1 = y.max(0);
    let x2 = (x + w - 1).min(buf.width() as i32 - 1);
    let y2 = (y + h - 1).min(buf.height() as i32 - 1);
    if x1 <= x2 && y1 <= y2 {
        buf.add_damage(x1, y1, x2, y2);
    }
}

pub fn draw_line(buf: &mut DrawBuffer, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    draw_primitives::line(buf, x0, y0, x1, y1, color);
    let min_x = x0.min(x1).max(0);
    let min_y = y0.min(y1).max(0);
    let max_x = x0.max(x1).min(buf.width() as i32 - 1);
    let max_y = y0.max(y1).min(buf.height() as i32 - 1);
    buf.add_damage(min_x, min_y, max_x, max_y);
}

pub fn draw_circle(buf: &mut DrawBuffer, cx: i32, cy: i32, radius: i32, color: u32) {
    draw_primitives::circle(buf, cx, cy, radius, color);
    let min_x = (cx - radius).max(0);
    let min_y = (cy - radius).max(0);
    let max_x = (cx + radius).min(buf.width() as i32 - 1);
    let max_y = (cy + radius).min(buf.height() as i32 - 1);
    buf.add_damage(min_x, min_y, max_x, max_y);
}

pub fn draw_circle_filled(buf: &mut DrawBuffer, cx: i32, cy: i32, radius: i32, color: u32) {
    draw_primitives::circle_filled(buf, cx, cy, radius, color);
    let min_x = (cx - radius).max(0);
    let min_y = (cy - radius).max(0);
    let max_x = (cx + radius).min(buf.width() as i32 - 1);
    let max_y = (cy + radius).min(buf.height() as i32 - 1);
    buf.add_damage(min_x, min_y, max_x, max_y);
}

pub fn draw_rect(buf: &mut DrawBuffer, x: i32, y: i32, w: i32, h: i32, color: u32) {
    draw_primitives::rect(buf, x, y, w, h, color);
    let x1 = x.max(0);
    let y1 = y.max(0);
    let x2 = (x + w - 1).min(buf.width() as i32 - 1);
    let y2 = (y + h - 1).min(buf.height() as i32 - 1);
    if x1 <= x2 && y1 <= y2 {
        buf.add_damage(x1, y1, x2, y2);
    }
}

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

    let src_x0 = src_x.max(0);
    let src_y0 = src_y.max(0);
    let src_x1 = (src_x + width - 1).min(buf_width - 1);
    let src_y1 = (src_y + height - 1).min(buf_height - 1);

    if src_x0 > src_x1 || src_y0 > src_y1 {
        return;
    }

    let actual_width = (src_x1 - src_x0 + 1) as usize;
    let actual_height = (src_y1 - src_y0 + 1) as usize;

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

    if dst_y0 < src_y0 || (dst_y0 == src_y0 && dst_x0 < src_x0) {
        for row in 0..copy_height {
            let src_off = ((src_y0 as usize + row) * pitch) + (src_x0 as usize * bytes_pp);
            let dst_off = ((dst_y0 as usize + row) * pitch) + (dst_x0 as usize * bytes_pp);
            data.copy_within(src_off..src_off + row_bytes, dst_off);
        }
    } else {
        for row in (0..copy_height).rev() {
            let src_off = ((src_y0 as usize + row) * pitch) + (src_x0 as usize * bytes_pp);
            let dst_off = ((dst_y0 as usize + row) * pitch) + (dst_x0 as usize * bytes_pp);
            data.copy_within(src_off..src_off + row_bytes, dst_off);
        }
    }

    buf.add_damage(dst_x0, dst_y0, dst_x1, dst_y1);
}

pub fn scroll_up(buf: &mut DrawBuffer, pixels: i32, fill_color: u32) {
    if pixels <= 0 {
        return;
    }

    let height = buf.height() as i32;
    let width = buf.width() as i32;

    if pixels >= height {
        let raw = buf.pixel_format().convert_color(fill_color);
        buf.clear(raw);
        return;
    }

    blit(buf, 0, pixels, 0, 0, width, height - pixels);
    fill_rect(buf, 0, height - pixels, width, pixels, fill_color);
}

pub fn scroll_down(buf: &mut DrawBuffer, pixels: i32, fill_color: u32) {
    if pixels <= 0 {
        return;
    }

    let height = buf.height() as i32;
    let width = buf.width() as i32;

    if pixels >= height {
        let raw = buf.pixel_format().convert_color(fill_color);
        buf.clear(raw);
        return;
    }

    blit(buf, 0, 0, 0, pixels, width, height - pixels);
    fill_rect(buf, 0, 0, width, pixels, fill_color);
}
