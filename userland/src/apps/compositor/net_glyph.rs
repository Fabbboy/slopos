//! The only code that turns a [`GlyphSpec`] into pixels, so the bar and the
//! network panel share one icon at different scales.
//!
//! The badge is cut into the glyph rather than pasted on top — moat, badge,
//! anti-aliased outline — because without the moat the badge sits on the bus
//! and the drops and the icon reads as a smudge at 14 px. Its palette is the
//! window buttons': amber for a warning, red for a fault.

use slopos_chrome_core::glyph::{Badge, GLYPH_H, GLYPH_W, GlyphSpec, Ink};
use slopos_gfx::canvas_ops::{circle_aa, circle_filled, fill_rect_clipped, line_aa};

use crate::gfx::{DamageRect, DrawBuffer};
use crate::theme::*;

const BADGE_MOAT_RADIUS: i32 = 4;

const BADGE_RADIUS: i32 = 3;

pub const fn glyph_width(scale: i32) -> i32 {
    GLYPH_W * scale
}

pub const fn glyph_height(scale: i32) -> i32 {
    GLYPH_H * scale
}

/// The circles and lines below take no clip argument of their own, so the
/// buffer's scissor is narrowed to `clip` for the duration.
pub fn draw(buf: &mut DrawBuffer, spec: &GlyphSpec, x: i32, y: i32, scale: i32, clip: &DamageRect) {
    if scale <= 0 {
        return;
    }
    let ink = ink_color(spec.ink);
    let rects = spec.rects();

    buf.with_scissor(*clip, |buf| {
        for r in rects {
            fill_rect_clipped(
                buf,
                x + r.x * scale,
                y + r.y * scale,
                r.w * scale,
                r.h * scale,
                ink,
                clip,
            );
        }
        draw_badge(buf, spec.badge, x, y, scale);
    });
}

fn ink_color(ink: Ink) -> slopos_abi::draw::Color32 {
    match ink {
        Ink::Ok => TEXT_PRIMARY,
        Ink::Transient => TEXT_SECONDARY,
        Ink::Down => TEXT_DISABLED,
    }
}

fn draw_badge(buf: &mut DrawBuffer, badge: Badge, x: i32, y: i32, scale: i32) {
    match badge {
        Badge::None => {}
        Badge::Warn => draw_dot_badge(buf, x, y, scale, SIGNAL_MINIMIZE),
        Badge::Error => draw_dot_badge(buf, x, y, scale, SIGNAL_CLOSE),
        Badge::Slash => draw_slash(buf, x, y, scale),
    }
}

/// The centre is inset by the moat radius on both axes so the moat lands
/// inside the glyph's box; an overhanging badge would be clipped by the
/// neighbouring slot's damage rect rather than by anything visible.
fn draw_dot_badge(
    buf: &mut DrawBuffer,
    x: i32,
    y: i32,
    scale: i32,
    color: slopos_abi::draw::Color32,
) {
    let cx = x + glyph_width(scale) - BADGE_MOAT_RADIUS;
    let cy = y + glyph_height(scale) - BADGE_MOAT_RADIUS;

    circle_filled(buf, cx, cy, BADGE_MOAT_RADIUS, PANEL_BG_OPAQUE);
    circle_filled(buf, cx, cy, BADGE_RADIUS, color);
    circle_aa(buf, cx, cy, BADGE_RADIUS, color);
}

/// Outlined in the bar background so it separates from the shape beneath it.
/// Muted ink rather than the fault palette: a switched-off network is a choice
/// someone made, and colouring it like a failure sends them debugging nothing.
fn draw_slash(buf: &mut DrawBuffer, x: i32, y: i32, scale: i32) {
    let x0 = x - 1;
    let y0 = y - 1;
    let x1 = x + glyph_width(scale);
    let y1 = y + glyph_height(scale);

    for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        line_aa(buf, x0 + dx, y0 + dy, x1 + dx, y1 + dy, PANEL_BG_OPAQUE);
    }
    line_aa(buf, x0, y0, x1, y1, TEXT_SECONDARY);
}
