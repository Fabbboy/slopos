//! System bar rendering and state for the SlopOS compositor.
//!
//! The system bar is the 24 px strip at the top of the screen: a system icon
//! and the active application's name on the left, status items packed against
//! the right edge.
//!
//! Hover repaints nothing here — [`super::hover::HoverRegistry`] already emits
//! both the old and the new rect when a hover flips.

use slopos_abi::draw::Color32;
use slopos_chrome_core::status::{StatusKind, StatusLayout, hit_status_item, layout_status_items};
use slopos_font::FontRenderer;

use super::hover::HOVER_STATUS_ITEM_BASE;
use super::status_item::{STATUS_ITEM_COUNT, StatusItems, hover_rect};
use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::theme::*;

/// Font size used when rendering with the TrueType font renderer.
const BAR_FONT_SIZE: u16 = 13;

const ICON_RADIUS: i32 = SYSTEM_BAR_ICON_SIZE / 2;

const BAR_BG: Color32 = Color32::new(
    PANEL_BG.red(),
    PANEL_BG.green(),
    PANEL_BG.blue(),
    PANEL_BG_ALPHA,
);

const ELLIPSIS: &str = "...";

/// Damage rects [`SystemBar::take_damage`] can emit in one frame: one per
/// item, or the single spanning rect a layout change produces.
pub const MAX_BAR_DAMAGE: usize = STATUS_ITEM_COUNT;

/// A cursor position no bar pixel can hold, for geometry-only layouts.
const NO_CURSOR: i32 = i32::MIN;

pub struct SystemBar {
    items: StatusItems,
    /// Layout digest as of the last damage pass. A change means items moved.
    last_signature: u64,
    /// Per-item content revisions as of the last damage pass.
    last_revision: [u32; STATUS_ITEM_COUNT],
    /// Left edge of the leftmost item as of the last damage pass, so a layout
    /// change also repaints the strip the items *used* to occupy.
    last_leftmost: i32,
    /// Screen width the last damage pass laid out against. Every slot moves
    /// with it, so a change is a layout change even at an equal signature.
    last_screen_width: i32,
}

impl SystemBar {
    pub fn new() -> Self {
        Self {
            items: StatusItems::new(),
            last_signature: 0,
            last_revision: [0; STATUS_ITEM_COUNT],
            last_leftmost: i32::MAX,
            last_screen_width: 0,
        }
    }

    /// Publish the network indicator's state; an unchanged rendered state bumps
    /// no revision and therefore produces no damage.
    pub fn set_network(&mut self, present: bool, state: slopos_chrome_core::NetIndicatorState) {
        self.items.set_network(present, state);
    }

    /// Where the status items sit — the single geometry source, for draw,
    /// hit-test, hover and damage alike.
    fn layout(&self, screen_width: u32, cursor_x: i32, cursor_y: i32) -> StatusLayout {
        layout_status_items(self.items.specs(), screen_width as i32, cursor_x, cursor_y)
    }

    /// Render the system bar onto the buffer.
    ///
    /// `active_app_name`: title of the focused window, or "SlopOS" if none.
    /// `cursor_x` / `cursor_y`: the pointer, for the hovered item's backdrop.
    pub fn draw(
        &self,
        buf: &mut DrawBuffer,
        screen_width: u32,
        active_app_name: &str,
        cursor_x: i32,
        cursor_y: i32,
        font: Option<&mut FontRenderer>,
        clip: Option<DamageRect>,
    ) {
        let sw = screen_width as i32;
        let bar_h = SYSTEM_BAR_HEIGHT;

        let full_clip = DamageRect {
            x0: 0,
            y0: 0,
            x1: sw - 1,
            y1: bar_h, // includes the 1 px border row
        };
        let clip = clip.unwrap_or(full_clip);

        gfx::fill_rect_blended_clipped(buf, 0, 0, sw, bar_h, BAR_BG, &clip);
        gfx::fill_rect_clipped(buf, 0, bar_h, sw, 1, PANEL_BORDER, &clip);

        let layout = self.layout(screen_width, cursor_x, cursor_y);

        let mut cursor = SYSTEM_BAR_PADDING_X;

        let icon_cx = cursor + ICON_RADIUS;
        let icon_cy = bar_h / 2;
        gfx::draw_circle_filled(buf, icon_cx, icon_cy, ICON_RADIUS, SIGNAL_EXPAND);
        cursor += SYSTEM_BAR_ICON_SIZE + SYSTEM_BAR_ICON_GAP;

        let name = if active_app_name.is_empty() {
            "SlopOS"
        } else {
            active_app_name
        };
        let text_y = (bar_h - BAR_FONT_SIZE as i32) / 2;
        let name_budget = (layout.app_name_limit - cursor).min(SYSTEM_BAR_MAX_APP_NAME_WIDTH);

        if name_budget > 0 {
            if let Some(font) = font {
                draw_name_ttf(buf, font, cursor, text_y, name, name_budget);
            } else {
                draw_name_bitmap(buf, cursor, text_y, name, name_budget, &clip);
            }
        }

        for (index, slot) in layout.slots().iter().enumerate() {
            self.items
                .draw(buf, slot, layout.hovered == Some(index), &clip);
        }
    }

    /// Whether `py` falls inside the bar strip.  The bar spans the full width,
    /// so x says nothing; which *item* it is over is [`Self::hit_test`].
    pub fn covers(py: i32) -> bool {
        py >= 0 && py < SYSTEM_BAR_HEIGHT
    }

    /// Which status item `(px, py)` lands on.  Laid out at the passed position,
    /// so a click batched with the motion that brought the cursor here is
    /// tested against where the cursor now is.
    pub fn hit_test(&self, screen_width: u32, px: i32, py: i32) -> Option<StatusKind> {
        let layout = self.layout(screen_width, px, py);
        hit_status_item(&layout, px, py)
    }

    /// The screen rect a status item occupies, for anchoring a popover to it.
    ///
    /// Laid out through the same path as `draw` and `hit_test`, so a popover
    /// cannot be anchored to a rectangle the item is not drawn in.
    pub fn item_rect(&self, screen_width: u32, kind: StatusKind) -> Option<DamageRect> {
        let layout = self.layout(screen_width, NO_CURSOR, NO_CURSOR);
        layout.slot_for(kind).map(|slot| DamageRect {
            x0: slot.x,
            y0: 0,
            x1: slot.x + slot.w - 1,
            y1: SYSTEM_BAR_HEIGHT,
        })
    }

    /// The hover regions to register for this frame: one `(id, rect, hovered)`
    /// per placed item, from the same layout [`Self::draw`] uses. Returns how
    /// many were written.
    pub fn hover_regions(
        &self,
        screen_width: u32,
        cursor_x: i32,
        cursor_y: i32,
        out: &mut [(u32, DamageRect, bool)],
    ) -> usize {
        let layout = self.layout(screen_width, cursor_x, cursor_y);
        let mut count = 0usize;
        for (index, slot) in layout.slots().iter().enumerate() {
            if count >= out.len() {
                break;
            }
            out[count] = (
                HOVER_STATUS_ITEM_BASE | slot.kind as u32,
                hover_rect(slot),
                layout.hovered == Some(index),
            );
            count += 1;
        }
        count
    }

    /// The strip the system icon and the app-name text occupy.
    ///
    /// The bar's name changes with keyboard focus, and nothing else in the
    /// frame damages `y < 24` when focus moves — the title-bar damage covers
    /// the windows, not the bar — so without this the name stays stale until
    /// something unrelated repaints the strip.
    ///
    /// Bounded by the name's own width cap as well as by the leftmost status
    /// item: the text is truncated at [`SYSTEM_BAR_MAX_APP_NAME_WIDTH`], so
    /// repainting all the way out to `app_name_limit` would be most of a
    /// 1920 px bar to redraw a 200 px label.
    pub fn app_name_damage(&self, screen_width: u32) -> DamageRect {
        let layout = self.layout(screen_width, NO_CURSOR, NO_CURSOR);
        let text_x = SYSTEM_BAR_PADDING_X + SYSTEM_BAR_ICON_SIZE + SYSTEM_BAR_ICON_GAP;
        let budget = (layout.app_name_limit - text_x)
            .min(SYSTEM_BAR_MAX_APP_NAME_WIDTH)
            .max(0);
        DamageRect {
            x0: SYSTEM_BAR_PADDING_X,
            y0: 0,
            x1: text_x + budget,
            y1: SYSTEM_BAR_HEIGHT,
        }
    }

    /// Fold `uptime_secs` into the clock and report what changed since the
    /// previous call. Returns how many rects were written to `out`, which must
    /// hold at least [`MAX_BAR_DAMAGE`].
    ///
    /// Three tiers, cheapest last:
    ///
    /// - **Layout change** — an item appeared, vanished, changed width, or the
    ///   screen resized. Everything left of the changed item moved, so one rect
    ///   spans from the leftmost of (previous, current) to the right edge.
    /// - **Content change** — that item's slot alone.
    /// - **Nothing** — no rects.
    pub fn take_damage(
        &mut self,
        screen_width: u32,
        uptime_secs: u64,
        out: &mut [DamageRect],
    ) -> usize {
        self.items.set_uptime(uptime_secs);

        let sw = screen_width as i32;
        let layout = self.layout(screen_width, NO_CURSOR, NO_CURSOR);
        let leftmost = layout
            .slots()
            .iter()
            .map(|slot| slot.x)
            .min()
            .unwrap_or(sw - SYSTEM_BAR_PADDING_X);

        let mut count = 0usize;
        let layout_changed =
            layout.signature != self.last_signature || sw != self.last_screen_width;

        if layout_changed {
            let x0 = leftmost.min(self.last_leftmost).max(0);
            push(
                out,
                &mut count,
                DamageRect {
                    x0,
                    y0: 0,
                    x1: sw - 1,
                    y1: SYSTEM_BAR_HEIGHT,
                },
            );
        } else {
            let revisions = self.items.revisions();
            for slot in layout.slots() {
                if revisions[slot.idx] != self.last_revision[slot.idx] {
                    push(
                        out,
                        &mut count,
                        DamageRect {
                            x0: slot.x,
                            y0: 0,
                            x1: slot.x + slot.w - 1,
                            y1: SYSTEM_BAR_HEIGHT,
                        },
                    );
                }
            }
        }

        self.last_signature = layout.signature;
        self.last_screen_width = sw;
        self.last_leftmost = leftmost;
        self.last_revision = self.items.revisions();
        count
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn push(out: &mut [DamageRect], count: &mut usize, rect: DamageRect) {
    if *count < out.len() {
        out[*count] = rect;
        *count += 1;
    }
}

/// Draw the active app name using the TrueType font, truncating with "..."
/// if it exceeds `max_w`.
fn draw_name_ttf(
    buf: &mut DrawBuffer,
    font: &mut FontRenderer,
    x: i32,
    y: i32,
    name: &str,
    max_w: i32,
) {
    let (full_w, _) = font.measure_text(name, BAR_FONT_SIZE);

    if full_w <= max_w {
        font.draw_text(
            buf,
            x,
            y,
            name,
            BAR_FONT_SIZE,
            TEXT_PRIMARY,
            PANEL_BG_OPAQUE,
        );
        return;
    }

    // Find the longest prefix that fits together with "...".
    let (ell_w, _) = font.measure_text(ELLIPSIS, BAR_FONT_SIZE);
    let budget = max_w - ell_w;
    let prefix_len = find_prefix_len(font, name, budget);
    let prefix = &name[..prefix_len];

    font.draw_text(
        buf,
        x,
        y,
        prefix,
        BAR_FONT_SIZE,
        TEXT_PRIMARY,
        PANEL_BG_OPAQUE,
    );
    let (pw, _) = font.measure_text(prefix, BAR_FONT_SIZE);
    font.draw_text(
        buf,
        x + pw,
        y,
        ELLIPSIS,
        BAR_FONT_SIZE,
        TEXT_PRIMARY,
        PANEL_BG_OPAQUE,
    );
}

/// Find the longest byte-aligned prefix of `name` whose rendered width
/// (at `BAR_FONT_SIZE`) does not exceed `budget` pixels.
fn find_prefix_len(font: &FontRenderer, name: &str, budget: i32) -> usize {
    let mut best = 0;
    for (i, _) in name.char_indices() {
        if i == 0 {
            continue;
        }
        let (w, _) = font.measure_text(&name[..i], BAR_FONT_SIZE);
        if w > budget {
            break;
        }
        best = i;
    }
    best
}

/// Draw the active app name using the bitmap fallback font, truncating with
/// "..." if it exceeds `max_w`.
fn draw_name_bitmap(
    buf: &mut DrawBuffer,
    x: i32,
    y: i32,
    name: &str,
    max_w: i32,
    clip: &DamageRect,
) {
    let full_w = gfx::font::string_width(name);

    if full_w <= max_w {
        gfx::draw_str_clipped(buf, x, y, name, TEXT_PRIMARY, PANEL_BG_OPAQUE, clip);
        return;
    }

    let ell_w = gfx::font::string_width(ELLIPSIS);
    let budget = max_w - ell_w;
    let prefix_len = find_bitmap_prefix_len(name, budget);
    let prefix = &name[..prefix_len];

    gfx::draw_str_clipped(buf, x, y, prefix, TEXT_PRIMARY, PANEL_BG_OPAQUE, clip);
    let pw = gfx::font::string_width(prefix);
    gfx::draw_str_clipped(
        buf,
        x + pw,
        y,
        ELLIPSIS,
        TEXT_PRIMARY,
        PANEL_BG_OPAQUE,
        clip,
    );
}

/// Find the longest byte-aligned prefix whose bitmap-font width fits in
/// `budget` pixels.
fn find_bitmap_prefix_len(name: &str, budget: i32) -> usize {
    let mut best = 0;
    for (i, _) in name.char_indices() {
        if i == 0 {
            continue;
        }
        let w = gfx::font::string_width(&name[..i]);
        if w > budget {
            break;
        }
        best = i;
    }
    best
}
