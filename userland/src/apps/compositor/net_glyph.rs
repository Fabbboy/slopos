//! The only code that turns a [`GlyphSpec`] into pixels.
//!
//! `slopos_chrome_core::glyph` says what the network indicator looks like as
//! rectangles in a unit grid; this draws them. One rasteriser is what lets the
//! bar and the network panel show the same icon at different scales without
//! either growing its own copy of the shape.
//!
//! The badge is cut into the glyph rather than pasted on top: a
//! background-coloured moat is drawn first, then the badge inside it, then an
//! anti-aliased outline to feather the moat's edge. Without the moat the badge
//! sits on the bus and the drops and the whole icon reads as a smudge at 14 px.
//! Its palette is the window buttons' — amber for a warning, red for a fault —
//! so the bar and the title bars speak one colour language.

use slopos_chrome_core::glyph::{Badge, GLYPH_H, GLYPH_W, GlyphSpec, Ink};
use slopos_gfx::canvas_ops::{circle_aa, circle_filled, fill_rect_clipped, line_aa};

use crate::gfx::{DamageRect, DrawBuffer};
use crate::theme::*;

/// Radius of the background moat separating the badge from the shape.
const BADGE_MOAT_RADIUS: i32 = 4;

/// Radius of the badge itself.
const BADGE_RADIUS: i32 = 3;

/// Width of the glyph as drawn at `scale`.
pub const fn glyph_width(scale: i32) -> i32 {
    GLYPH_W * scale
}

/// Height of the glyph as drawn at `scale`.
pub const fn glyph_height(scale: i32) -> i32 {
    GLYPH_H * scale
}

/// Draw `spec` with its top-left at `(x, y)`, scaled by an integer `scale`.
///
/// Every primitive is confined to `clip`: the circles and lines below take no
/// clip argument of their own, so the buffer's scissor is narrowed for the
/// duration rather than trusting the caller to have set one.
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

/// A dot in the glyph's bottom-right corner, moat first.
///
/// The centre is inset by the moat radius on both axes so the moat lands
/// entirely inside the glyph's own box — the item's width is the glyph's
/// width, and a badge that overhung it would be clipped by the neighbouring
/// slot's damage rect rather than by anything visible.
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

/// A diagonal stroke across the whole glyph, outlined in the bar background so
/// it separates from the shape beneath it.
///
/// Muted ink, not the fault palette: a switched-off network is a choice
/// someone made, and colouring it like a failure sends them debugging a
/// non-problem.
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
