//! Title bars with rounded corners, the signal buttons
//! (close / minimize / expand) and the hit-tests that map a cursor onto them.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::Color32;
use slopos_font::FontRenderer;
use slopos_gfx::canvas_ops::{circle_filled, line_aa};

use crate::gfx::{self, DrawBuffer};
use crate::theme;

const TITLE_FONT_SIZE: u16 = 14;

/// Half-length of the close/expand cross/plus glyph arms.
const GLYPH_HALF: i32 = theme::SIGNAL_GLYPH_SIZE / 2;
const MINIMIZE_DASH_HALF: i32 = 4;

/// Render the title bar, signal buttons, and window frame for one window.
///
/// `signal_hovered` is whether the cursor is inside the signal-button group box.
pub fn draw_window_decorations(
    buf: &mut DrawBuffer,
    window_x: i32,
    window_y: i32,
    window_w: u32,
    window_h: u32,
    title: &str,
    focused: bool,
    signal_hovered: bool,
    font: Option<&mut FontRenderer>,
    clip: Option<DamageRect>,
) {
    let full_clip = DamageRect {
        x0: 0,
        y0: 0,
        x1: buf.width() as i32 - 1,
        y1: buf.height() as i32 - 1,
    };
    let clip = clip.unwrap_or(full_clip);

    draw_title_bar_background(buf, window_x, window_y, window_w, window_h, focused, &clip);
    draw_signal_buttons(buf, window_x, window_y, focused, signal_hovered, &clip);
    draw_title_text(
        buf, window_x, window_y, window_w, title, focused, font, &clip,
    );
}

/// Hit-test the signal button group: `Some(0)` = close, `Some(1)` = minimize,
/// `Some(2)` = expand, `None` = miss.
pub fn hit_test_signal_button(window_x: i32, window_y: i32, px: i32, py: i32) -> Option<u8> {
    let buttons: [(i32, u8); 3] = [
        (window_x + theme::SIGNAL_BUTTON_1_CX, 0), // close
        (window_x + theme::SIGNAL_BUTTON_2_CX, 1), // minimize
        (window_x + theme::SIGNAL_BUTTON_3_CX, 2), // expand
    ];
    let cy = window_y + theme::SIGNAL_BUTTON_CY;

    for &(cx, id) in &buttons {
        let dx = px - cx;
        let dy = py - cy;
        if dx * dx + dy * dy <= theme::SIGNAL_BUTTON_RADIUS * theme::SIGNAL_BUTTON_RADIUS {
            return Some(id);
        }
    }
    None
}

pub fn hit_test_signal_group(window_x: i32, window_y: i32, px: i32, py: i32) -> bool {
    let gx = window_x + theme::SIGNAL_GROUP_X;
    let gy = window_y + theme::SIGNAL_GROUP_Y;
    px >= gx && px < gx + theme::SIGNAL_GROUP_W && py >= gy && py < gy + theme::SIGNAL_GROUP_H
}

/// Detect if `(px, py)` is in a resize grab zone: the shadow region outside the
/// window frame, with corners taking priority over edges.
///
/// `window_y` is the content top (kernel's `window.y`); the title bar extends
/// above it.
pub fn hit_test_resize_edge(
    window_x: i32,
    window_y: i32,
    window_w: u32,
    window_h: u32,
    px: i32,
    py: i32,
) -> super::input::ResizeEdge {
    use super::input::ResizeEdge;

    let ww = window_w as i32;
    let wh = window_h as i32;

    let frame_x0 = window_x;
    let frame_y0 = window_y - theme::TITLE_BAR_HEIGHT;
    let frame_x1 = window_x + ww - 1;
    let frame_y1 = window_y + wh - 1;

    let spread = theme::SHADOW_SPREAD;
    let ext_x0 = frame_x0 - spread;
    let ext_y0 = frame_y0 - spread;
    let ext_x1 = frame_x1 + spread;
    let ext_y1 = frame_y1 + spread;

    if px < ext_x0 || px > ext_x1 || py < ext_y0 || py > ext_y1 {
        return ResizeEdge::NONE;
    }

    if px >= frame_x0 && px <= frame_x1 && py >= frame_y0 && py <= frame_y1 {
        return ResizeEdge::NONE;
    }

    let frame_w = ww;
    let frame_h = wh + theme::TITLE_BAR_HEIGHT;
    let corner_range = {
        let r = theme::WINDOW_CORNER_RADIUS * 3; // ~24px
        let max_w = frame_w / 2;
        let max_h = frame_h / 2;
        let mut cr = r;
        if cr > max_w {
            cr = max_w;
        }
        if cr > max_h {
            cr = max_h;
        }
        cr
    };

    let near_left = px < frame_x0 + corner_range;
    let near_right = px > frame_x1 - corner_range;
    let near_top = py < frame_y0 + corner_range;
    let near_bottom = py > frame_y1 - corner_range;

    if near_top && near_left {
        return ResizeEdge::TOP_LEFT;
    }
    if near_top && near_right {
        return ResizeEdge::TOP_RIGHT;
    }
    if near_bottom && near_left {
        return ResizeEdge::BOTTOM_LEFT;
    }
    if near_bottom && near_right {
        return ResizeEdge::BOTTOM_RIGHT;
    }

    if py < frame_y0 {
        return ResizeEdge::TOP;
    }
    if py > frame_y1 {
        return ResizeEdge::BOTTOM;
    }
    if px < frame_x0 {
        return ResizeEdge::LEFT;
    }
    if px > frame_x1 {
        return ResizeEdge::RIGHT;
    }

    ResizeEdge::NONE
}

/// Draw the opaque title-bar background with rounded top corners.
///
/// The corner pixels are never written, so whatever is already in the
/// framebuffer stays visible through the rounded corner.
fn draw_title_bar_background(
    buf: &mut DrawBuffer,
    window_x: i32,
    window_y: i32,
    window_w: u32,
    _window_h: u32,
    focused: bool,
    clip: &DamageRect,
) {
    let bg = if focused {
        theme::TITLE_BAR_FOCUSED
    } else {
        theme::TITLE_BAR_UNFOCUSED
    };

    let w = window_w as i32;
    let bar_rect = DamageRect {
        x0: window_x,
        y0: window_y,
        x1: window_x + w - 1,
        y1: window_y + theme::TITLE_BAR_HEIGHT - 1,
    };
    if !rects_intersect(&bar_rect, clip) {
        return;
    }

    let r = theme::WINDOW_CORNER_RADIUS
        .max(0)
        .min(theme::TITLE_BAR_HEIGHT);

    if r > 0 && 2 * r <= w {
        let cy = window_y + r;
        let r_sq = r * r;
        for row in 0..r {
            let y = window_y + row;
            if y < clip.y0 || y > clip.y1 {
                continue;
            }
            let dy = cy - y;
            let x_off = isqrt_i32(r_sq - dy * dy);
            let fill_x0 = (window_x + r - x_off).max(clip.x0);
            let fill_x1 = (window_x + w - 1 - r + x_off).min(clip.x1);
            if fill_x0 <= fill_x1 {
                gfx::fill_rect(buf, fill_x0, y, fill_x1 - fill_x0 + 1, 1, bg);
            }
        }
    }

    let body_y0 = window_y + r;
    let body_y1 = window_y + theme::TITLE_BAR_HEIGHT - 1;
    if body_y0 <= body_y1 {
        gfx::fill_rect_clipped(buf, window_x, body_y0, w, body_y1 - body_y0 + 1, bg, clip);
    }
}

/// Draw the three signal buttons (close, minimize, expand).
fn draw_signal_buttons(
    buf: &mut DrawBuffer,
    window_x: i32,
    window_y: i32,
    focused: bool,
    signal_hovered: bool,
    clip: &DamageRect,
) {
    let button_cy = window_y + theme::SIGNAL_BUTTON_CY;

    let (close_color, min_color, expand_color) = if focused {
        (
            theme::SIGNAL_CLOSE,
            theme::SIGNAL_MINIMIZE,
            theme::SIGNAL_EXPAND,
        )
    } else {
        (
            theme::SIGNAL_INACTIVE,
            theme::SIGNAL_INACTIVE,
            theme::SIGNAL_INACTIVE,
        )
    };

    let buttons: [(i32, Color32); 3] = [
        (window_x + theme::SIGNAL_BUTTON_1_CX, close_color),
        (window_x + theme::SIGNAL_BUTTON_2_CX, min_color),
        (window_x + theme::SIGNAL_BUTTON_3_CX, expand_color),
    ];

    for &(cx, color) in &buttons {
        let bx0 = cx - theme::SIGNAL_BUTTON_RADIUS;
        let by0 = button_cy - theme::SIGNAL_BUTTON_RADIUS;
        let bx1 = cx + theme::SIGNAL_BUTTON_RADIUS;
        let by1 = button_cy + theme::SIGNAL_BUTTON_RADIUS;
        let btn_rect = DamageRect {
            x0: bx0,
            y0: by0,
            x1: bx1,
            y1: by1,
        };
        if !rects_intersect(&btn_rect, clip) {
            continue;
        }
        circle_filled(buf, cx, button_cy, theme::SIGNAL_BUTTON_RADIUS, color);
    }

    if focused && signal_hovered {
        draw_signal_glyphs(buf, window_x, window_y, clip);
    }
}

/// Draw the interior glyphs on the signal buttons (X, -, +).
fn draw_signal_glyphs(buf: &mut DrawBuffer, window_x: i32, window_y: i32, clip: &DamageRect) {
    let glyph_color = Color32::new(
        theme::SIGNAL_GLYPH.red(),
        theme::SIGNAL_GLYPH.green(),
        theme::SIGNAL_GLYPH.blue(),
        theme::SIGNAL_GLYPH_ALPHA,
    );
    let cy = window_y + theme::SIGNAL_BUTTON_CY;

    let close_cx = window_x + theme::SIGNAL_BUTTON_1_CX;
    let close_rect = DamageRect {
        x0: close_cx - GLYPH_HALF,
        y0: cy - GLYPH_HALF,
        x1: close_cx + GLYPH_HALF,
        y1: cy + GLYPH_HALF,
    };
    if rects_intersect(&close_rect, clip) {
        line_aa(
            buf,
            close_cx - GLYPH_HALF,
            cy - GLYPH_HALF,
            close_cx + GLYPH_HALF,
            cy + GLYPH_HALF,
            glyph_color,
        );
        line_aa(
            buf,
            close_cx + GLYPH_HALF,
            cy - GLYPH_HALF,
            close_cx - GLYPH_HALF,
            cy + GLYPH_HALF,
            glyph_color,
        );
    }

    let min_cx = window_x + theme::SIGNAL_BUTTON_2_CX;
    let min_rect = DamageRect {
        x0: min_cx - MINIMIZE_DASH_HALF,
        y0: cy,
        x1: min_cx + MINIMIZE_DASH_HALF,
        y1: cy,
    };
    if rects_intersect(&min_rect, clip) {
        line_aa(
            buf,
            min_cx - MINIMIZE_DASH_HALF,
            cy,
            min_cx + MINIMIZE_DASH_HALF,
            cy,
            glyph_color,
        );
    }

    let exp_cx = window_x + theme::SIGNAL_BUTTON_3_CX;
    let exp_rect = DamageRect {
        x0: exp_cx - MINIMIZE_DASH_HALF,
        y0: cy - MINIMIZE_DASH_HALF,
        x1: exp_cx + MINIMIZE_DASH_HALF,
        y1: cy + MINIMIZE_DASH_HALF,
    };
    if rects_intersect(&exp_rect, clip) {
        line_aa(
            buf,
            exp_cx - MINIMIZE_DASH_HALF,
            cy,
            exp_cx + MINIMIZE_DASH_HALF,
            cy,
            glyph_color,
        );
        line_aa(
            buf,
            exp_cx,
            cy - MINIMIZE_DASH_HALF,
            exp_cx,
            cy + MINIMIZE_DASH_HALF,
            glyph_color,
        );
    }
}

/// Draw the title text, centered in the title bar.
fn draw_title_text(
    buf: &mut DrawBuffer,
    window_x: i32,
    window_y: i32,
    window_w: u32,
    title: &str,
    focused: bool,
    font: Option<&mut FontRenderer>,
    clip: &DamageRect,
) {
    if title.is_empty() {
        return;
    }

    let (text_color, bg_color) = if focused {
        (theme::TEXT_PRIMARY, theme::TITLE_BAR_FOCUSED)
    } else {
        (theme::TEXT_SECONDARY, theme::TITLE_BAR_UNFOCUSED)
    };

    let max_text_w = (window_w as i32 - theme::TITLE_MAX_TEXT_WIDTH_MARGIN).max(0);
    if max_text_w == 0 {
        return;
    }

    if let Some(font) = font {
        draw_title_text_ttf(
            buf, window_x, window_y, window_w, title, text_color, bg_color, font, max_text_w, clip,
        );
    } else {
        draw_title_text_bitmap(
            buf, window_x, window_y, window_w, title, text_color, bg_color, max_text_w, clip,
        );
    }
}

fn draw_title_text_ttf(
    buf: &mut DrawBuffer,
    window_x: i32,
    window_y: i32,
    window_w: u32,
    title: &str,
    text_color: Color32,
    bg_color: Color32,
    font: &mut FontRenderer,
    max_text_w: i32,
    _clip: &DamageRect,
) {
    let (measured_w, measured_h) = font.measure_text(title, TITLE_FONT_SIZE);

    let (display_title, needs_ellipsis) = if measured_w > max_text_w {
        truncate_title_for_width(title, font, max_text_w)
    } else {
        (title, false)
    };

    let display_w = if needs_ellipsis {
        let (base_w, _) = font.measure_text(display_title, TITLE_FONT_SIZE);
        let (dots_w, _) = font.measure_text("...", TITLE_FONT_SIZE);
        base_w + dots_w
    } else {
        measured_w.min(max_text_w)
    };

    let text_x = window_x + (window_w as i32 - display_w) / 2;
    let text_y = window_y + (theme::TITLE_BAR_HEIGHT - measured_h.min(TITLE_FONT_SIZE as i32)) / 2;

    font.draw_text(
        buf,
        text_x,
        text_y,
        display_title,
        TITLE_FONT_SIZE,
        text_color,
        bg_color,
    );
    if needs_ellipsis {
        let (base_w, _) = font.measure_text(display_title, TITLE_FONT_SIZE);
        font.draw_text(
            buf,
            text_x + base_w,
            text_y,
            "...",
            TITLE_FONT_SIZE,
            text_color,
            bg_color,
        );
    }
}

/// Draw title text using the bitmap (VGA 8x16) fallback font.
fn draw_title_text_bitmap(
    buf: &mut DrawBuffer,
    window_x: i32,
    window_y: i32,
    window_w: u32,
    title: &str,
    text_color: Color32,
    bg_color: Color32,
    max_text_w: i32,
    clip: &DamageRect,
) {
    let cell_w = gfx::font::cell_width();
    let cell_h = gfx::font::cell_height();

    let max_chars = if cell_w > 0 {
        (max_text_w / cell_w) as usize
    } else {
        return;
    };

    if max_chars == 0 {
        return;
    }

    let title_bytes = title.as_bytes();
    let title_len = title_bytes.len();
    let (display_len, needs_ellipsis) = if title_len > max_chars {
        (max_chars.saturating_sub(3), true)
    } else {
        (title_len, false)
    };

    let display_w = if needs_ellipsis {
        (display_len + 3) as i32 * cell_w
    } else {
        display_len as i32 * cell_w
    };

    let text_x = window_x + (window_w as i32 - display_w) / 2;
    let text_y = window_y + (theme::TITLE_BAR_HEIGHT - cell_h) / 2;

    let safe_end = find_utf8_boundary(title, display_len);
    let display_str = &title[..safe_end];

    gfx::draw_str_clipped(buf, text_x, text_y, display_str, text_color, bg_color, clip);

    if needs_ellipsis {
        let dots_x = text_x + safe_end as i32 * cell_w;
        gfx::draw_str_clipped(buf, dots_x, text_y, "...", text_color, bg_color, clip);
    }
}

/// Truncate `title` so its rendered width with "..." appended fits `max_w`
/// pixels at `TITLE_FONT_SIZE`. Returns `(truncated_slice, needs_ellipsis)`.
fn truncate_title_for_width<'a>(
    title: &'a str,
    font: &FontRenderer,
    max_w: i32,
) -> (&'a str, bool) {
    let (dots_w, _) = font.measure_text("...", TITLE_FONT_SIZE);
    let target_w = (max_w - dots_w).max(0);

    let mut end = 0;
    for (i, ch) in title.char_indices() {
        let next = i + ch.len_utf8();
        let (w, _) = font.measure_text(&title[..next], TITLE_FONT_SIZE);
        if w > target_w {
            break;
        }
        end = next;
    }

    if end == 0 && !title.is_empty() {
        // At least show the ellipsis alone.
        return ("", true);
    }
    (&title[..end], true)
}

/// Find a UTF-8-safe byte boundary at or before `target` in `s`.
fn find_utf8_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Integer square root (floor).
fn isqrt_i32(n: i32) -> i32 {
    if n <= 0 {
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

fn rects_intersect(a: &DamageRect, b: &DamageRect) -> bool {
    a.x0 <= b.x1 && a.x1 >= b.x0 && a.y0 <= b.y1 && a.y1 >= b.y0
}
