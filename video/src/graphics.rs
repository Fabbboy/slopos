use crate::framebuffer::{self, FbState};

const GRAPHICS_SUCCESS: i32 = 0;
const GRAPHICS_ERROR_NO_FB: i32 = -1;
const GRAPHICS_ERROR_BOUNDS: i32 = -2;
const GRAPHICS_ERROR_INVALID: i32 = -3;

fn bytes_per_pixel(bpp: u8) -> u32 {
    ((bpp as u32) + 7) / 8
}

fn bounds_check(fb: &FbState, x: i32, y: i32) -> bool {
    x >= 0 && y >= 0 && (x as u32) < fb.width && (y as u32) < fb.height
}

fn clip_coords(fb: &FbState, x: &mut i32, y: &mut i32) {
    if *x < 0 {
        *x = 0;
    }
    if *y < 0 {
        *y = 0;
    }
    if *x >= fb.width as i32 {
        *x = fb.width.saturating_sub(1) as i32;
    }
    if *y >= fb.height as i32 {
        *y = fb.height.saturating_sub(1) as i32;
    }
}

fn convert_color(fb: &FbState, color: u32) -> u32 {
    match fb.pixel_format {
        0x02 | 0x04 => {
            ((color & 0xFF0000) >> 16) | (color & 0x00FF00) | ((color & 0x0000FF) << 16) | (color & 0xFF000000)
        }
        _ => color,
    }
}

#[no_mangle]
pub extern "C" fn graphics_draw_pixel(x: i32, y: i32, color: u32) -> i32 {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return GRAPHICS_ERROR_NO_FB,
    };

    if !bounds_check(&fb, x, y) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    framebuffer::framebuffer_set_pixel(x as u32, y as u32, color);
    GRAPHICS_SUCCESS
}

#[no_mangle]
pub extern "C" fn graphics_draw_hline(x1: i32, x2: i32, y: i32, color: u32) -> i32 {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return GRAPHICS_ERROR_NO_FB,
    };

    if !bounds_check(&fb, x1, y) && !bounds_check(&fb, x2, y) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    let (mut xa, mut xb) = if x1 > x2 { (x2, x1) } else { (x1, x2) };
    let mut y_clipped = y;
    clip_coords(&fb, &mut xa, &mut y_clipped);
    clip_coords(&fb, &mut xb, &mut y_clipped);

    for x in xa..=xb {
        framebuffer::framebuffer_set_pixel(x as u32, y_clipped as u32, color);
    }

    GRAPHICS_SUCCESS
}

#[no_mangle]
pub extern "C" fn graphics_draw_vline(x: i32, y1: i32, y2: i32, color: u32) -> i32 {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return GRAPHICS_ERROR_NO_FB,
    };

    if !bounds_check(&fb, x, y1) && !bounds_check(&fb, x, y2) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    let (mut ya, mut yb) = if y1 > y2 { (y2, y1) } else { (y1, y2) };
    let mut x_clipped = x;
    clip_coords(&fb, &mut x_clipped, &mut ya);
    clip_coords(&fb, &mut x_clipped, &mut yb);

    for y in ya..=yb {
        framebuffer::framebuffer_set_pixel(x_clipped as u32, y as u32, color);
    }

    GRAPHICS_SUCCESS
}

#[no_mangle]
pub extern "C" fn graphics_draw_line(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) -> i32 {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return GRAPHICS_ERROR_NO_FB,
    };

    let width = fb.width as i32;
    let height = fb.height as i32;
    if (x0 < 0 && x1 < 0) || (y0 < 0 && y1 < 0) || (x0 >= width && x1 >= width) || (y0 >= height && y1 >= height) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx - dy;

    let mut x = x0;
    let mut y = y0;
    loop {
        if bounds_check(&fb, x, y) {
            framebuffer::framebuffer_set_pixel(x as u32, y as u32, color);
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x += sx;
        }
        if e2 < dx {
            err += dx;
            y += sy;
        }
    }

    GRAPHICS_SUCCESS
}

#[no_mangle]
pub extern "C" fn graphics_draw_rect(x: i32, y: i32, width: i32, height: i32, color: u32) -> i32 {
    if width <= 0 || height <= 0 {
        return GRAPHICS_ERROR_INVALID;
    }

    if graphics_draw_hline(x, x + width - 1, y, color) == GRAPHICS_ERROR_NO_FB {
        return GRAPHICS_ERROR_NO_FB;
    }
    graphics_draw_hline(x, x + width - 1, y + height - 1, color);
    graphics_draw_vline(x, y, y + height - 1, color);
    graphics_draw_vline(x + width - 1, y, y + height - 1, color);
    GRAPHICS_SUCCESS
}

#[no_mangle]
pub extern "C" fn graphics_draw_rect_filled(x: i32, y: i32, width: i32, height: i32, color: u32) -> i32 {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return GRAPHICS_ERROR_NO_FB,
    };

    if width <= 0 || height <= 0 {
        return GRAPHICS_ERROR_INVALID;
    }

    let mut x1 = x;
    let mut y1 = y;
    let mut x2 = x + width - 1;
    let mut y2 = y + height - 1;

    if x1 < 0 {
        x1 = 0;
    }
    if y1 < 0 {
        y1 = 0;
    }
    if x2 >= fb.width as i32 {
        x2 = fb.width as i32 - 1;
    }
    if y2 >= fb.height as i32 {
        y2 = fb.height as i32 - 1;
    }

    if x1 > x2 || y1 > y2 {
        return GRAPHICS_ERROR_BOUNDS;
    }

    for row in y1..=y2 {
        for col in x1..=x2 {
            framebuffer::framebuffer_set_pixel(col as u32, row as u32, color);
        }
    }

    GRAPHICS_SUCCESS
}

#[no_mangle]
pub extern "C" fn graphics_draw_rect_filled_fast(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    color: u32,
) -> i32 {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return GRAPHICS_ERROR_NO_FB,
    };

    if width <= 0 || height <= 0 {
        return GRAPHICS_ERROR_INVALID;
    }

    let mut x1 = x;
    let mut y1 = y;
    let mut x2 = x + width - 1;
    let mut y2 = y + height - 1;

    if x1 < 0 {
        x1 = 0;
    }
    if y1 < 0 {
        y1 = 0;
    }
    if x2 >= fb.width as i32 {
        x2 = fb.width as i32 - 1;
    }
    if y2 >= fb.height as i32 {
        y2 = fb.height as i32 - 1;
    }

    if x1 > x2 || y1 > y2 {
        return GRAPHICS_ERROR_BOUNDS;
    }

    let pixel_value = convert_color(&fb, color);
    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let buffer = fb.base;
    let pitch = fb.pitch as usize;

    for row in y1..=y2 {
        let mut pixel_ptr = unsafe { buffer.add(row as usize * pitch + x1 as usize * bytes_pp) };
        if bytes_pp == 4 {
            let mut count = x2 - x1 + 1;
            while count > 0 {
                unsafe {
                    (pixel_ptr as *mut u32).write_volatile(pixel_value);
                    pixel_ptr = pixel_ptr.add(bytes_pp);
                }
                count -= 1;
            }
        } else {
            for _ in x1..=x2 {
                unsafe {
                    match bytes_pp {
                        2 => (pixel_ptr as *mut u16).write_volatile(pixel_value as u16),
                        3 => {
                            pixel_ptr.write_volatile(((pixel_value >> 16) & 0xFF) as u8);
                            pixel_ptr.add(1).write_volatile(((pixel_value >> 8) & 0xFF) as u8);
                            pixel_ptr.add(2).write_volatile((pixel_value & 0xFF) as u8);
                        }
                        _ => {}
                    }
                    pixel_ptr = pixel_ptr.add(bytes_pp);
                }
            }
        }
    }

    GRAPHICS_SUCCESS
}

#[no_mangle]
pub extern "C" fn graphics_draw_circle(cx: i32, cy: i32, radius: i32, color: u32) -> i32 {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return GRAPHICS_ERROR_NO_FB,
    };

    if radius <= 0 {
        return GRAPHICS_ERROR_INVALID;
    }

    let mut x = 0;
    let mut y = radius;
    let mut d = 1 - radius;

    if bounds_check(&fb, cx, cy + radius) {
        framebuffer::framebuffer_set_pixel(cx as u32, (cy + radius) as u32, color);
    }
    if bounds_check(&fb, cx, cy - radius) {
        framebuffer::framebuffer_set_pixel(cx as u32, (cy - radius) as u32, color);
    }
    if bounds_check(&fb, cx + radius, cy) {
        framebuffer::framebuffer_set_pixel((cx + radius) as u32, cy as u32, color);
    }
    if bounds_check(&fb, cx - radius, cy) {
        framebuffer::framebuffer_set_pixel((cx - radius) as u32, cy as u32, color);
    }

    while x < y {
        if d < 0 {
            d += 2 * x + 3;
        } else {
            d += 2 * (x - y) + 5;
            y -= 1;
        }
        x += 1;

        let points = [
            (cx + x, cy + y),
            (cx - x, cy + y),
            (cx + x, cy - y),
            (cx - x, cy - y),
            (cx + y, cy + x),
            (cx - y, cy + x),
            (cx + y, cy - x),
            (cx - y, cy - x),
        ];

        for (px, py) in points {
            if bounds_check(&fb, px, py) {
                framebuffer::framebuffer_set_pixel(px as u32, py as u32, color);
            }
        }
    }

    GRAPHICS_SUCCESS
}

#[no_mangle]
pub extern "C" fn graphics_draw_circle_filled(cx: i32, cy: i32, radius: i32, color: u32) -> i32 {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return GRAPHICS_ERROR_NO_FB,
    };

    if radius <= 0 {
        return GRAPHICS_ERROR_INVALID;
    }

    let radius_sq = radius * radius;
    for y in -radius..=radius {
        for x in -radius..=radius {
            if x * x + y * y <= radius_sq {
                let px = cx + x;
                let py = cy + y;
                if bounds_check(&fb, px, py) {
                    framebuffer::framebuffer_set_pixel(px as u32, py as u32, color);
                }
            }
        }
    }

    GRAPHICS_SUCCESS
}
