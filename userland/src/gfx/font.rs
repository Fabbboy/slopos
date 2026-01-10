use super::DrawBuffer;

pub use slopos_abi::font::{
    FONT_CHAR_COUNT, FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, FONT_DATA, FONT_FIRST_CHAR, FONT_LAST_CHAR,
    get_glyph,
};

/// Draw a single character to a buffer
pub fn draw_char(buf: &mut DrawBuffer, x: i32, y: i32, ch: u8, fg: u32, bg: u32) {
    let glyph = get_glyph(ch).unwrap_or_else(|| get_glyph(b' ').unwrap());

    let width = buf.width() as i32;
    let height = buf.height() as i32;

    for (row_idx, &row_bits) in glyph.iter().enumerate() {
        let py = y + row_idx as i32;
        if py < 0 || py >= height {
            continue;
        }
        for col in 0..FONT_CHAR_WIDTH {
            let px = x + col;
            if px < 0 || px >= width {
                continue;
            }
            let mask = 1u8 << (7 - col);
            let color = if (row_bits & mask) != 0 { fg } else { bg };
            buf.set_pixel(px, py, color);
        }
    }
}

/// Draw a string to a buffer, handling newlines and tabs
pub fn draw_string(buf: &mut DrawBuffer, x: i32, y: i32, text: &str, fg: u32, bg: u32) {
    let width = buf.width() as i32;
    let height = buf.height() as i32;

    let mut cx = x;
    let mut cy = y;

    let mut dirty_x0 = width;
    let mut dirty_y0 = height;
    let mut dirty_x1 = 0i32;
    let mut dirty_y1 = 0i32;

    for ch in text.bytes() {
        match ch {
            b'\n' => {
                cx = x;
                cy += FONT_CHAR_HEIGHT;
            }
            b'\r' => {
                cx = x;
            }
            b'\t' => {
                let tab_width = 4 * FONT_CHAR_WIDTH;
                cx = ((cx - x + tab_width) / tab_width) * tab_width + x;
            }
            _ => {
                draw_char(buf, cx, cy, ch, fg, bg);

                // Track damage
                let gx0 = cx.max(0);
                let gy0 = cy.max(0);
                let gx1 = (cx + FONT_CHAR_WIDTH - 1).min(width - 1);
                let gy1 = (cy + FONT_CHAR_HEIGHT - 1).min(height - 1);

                if gx0 <= gx1 && gy0 <= gy1 {
                    dirty_x0 = dirty_x0.min(gx0);
                    dirty_y0 = dirty_y0.min(gy0);
                    dirty_x1 = dirty_x1.max(gx1);
                    dirty_y1 = dirty_y1.max(gy1);
                }

                cx += FONT_CHAR_WIDTH;
                if cx + FONT_CHAR_WIDTH > width {
                    cx = x;
                    cy += FONT_CHAR_HEIGHT;
                }
            }
        }

        if cy >= height {
            break;
        }
    }

    if dirty_x0 <= dirty_x1 && dirty_y0 <= dirty_y1 {
        buf.add_damage(dirty_x0, dirty_y0, dirty_x1, dirty_y1);
    }
}

/// Calculate the width of a string in pixels
pub fn string_width(text: &str) -> i32 {
    let mut width = 0i32;
    for ch in text.bytes() {
        match ch {
            b'\n' => break,
            b'\t' => {
                let tab_width = 4 * FONT_CHAR_WIDTH;
                width = ((width + tab_width - 1) / tab_width) * tab_width;
            }
            _ => width += FONT_CHAR_WIDTH,
        }
    }
    width
}

/// Count the number of lines in a string
pub fn string_lines(text: &str) -> i32 {
    let mut lines = 1i32;
    for ch in text.bytes() {
        if ch == b'\n' {
            lines += 1;
        }
    }
    lines
}

/// Calculate the height of a string in pixels
pub fn string_height(text: &str) -> i32 {
    string_lines(text) * FONT_CHAR_HEIGHT
}
