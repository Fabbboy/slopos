//! System bar rendering and state for the SlopOS compositor.
//!
//! The system bar is the 24 px strip at the top of the screen. It displays
//! the active application name on the left and an uptime clock on the right,
//! separated by a small system icon (green dot).

use slopos_abi::draw::Color32;
use slopos_font::FontRenderer;

use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::theme::*;

// ---------------------------------------------------------------------------
// Constants derived from theme.rs (kept as local aliases for readability)
// ---------------------------------------------------------------------------

/// Font size used when rendering with the TrueType font renderer.
const BAR_FONT_SIZE: u16 = 13;

/// Fixed width reserved for the clock damage region on the right side.
const CLOCK_DAMAGE_WIDTH: i32 = 80;

/// Radius of the system icon circle (half of SYSTEM_BAR_ICON_SIZE).
const ICON_RADIUS: i32 = SYSTEM_BAR_ICON_SIZE / 2;

/// Opaque background colour derived from PANEL_BG for anti-alias blending
/// (used as the `bg` parameter in font rendering where read-back from a
/// semi-transparent panel would be inaccurate).
const OPAQUE_BAR_BG: Color32 =
    Color32::new(PANEL_BG.red(), PANEL_BG.green(), PANEL_BG.blue(), 0xFF);

/// Semi-transparent panel background colour (PANEL_BG + PANEL_BG_ALPHA).
const BAR_BG: Color32 = Color32::new(
    PANEL_BG.red(),
    PANEL_BG.green(),
    PANEL_BG.blue(),
    PANEL_BG_ALPHA,
);

/// Ellipsis string for truncation.
const ELLIPSIS: &str = "...";

// ---------------------------------------------------------------------------
// SystemBar
// ---------------------------------------------------------------------------

/// System bar state (lives in the compositor's main struct).
pub struct SystemBar {
    /// Cached clock string to detect changes for damage.
    last_clock: [u8; 8], // "HH:MM:SS"
}

impl SystemBar {
    pub fn new() -> Self {
        Self {
            last_clock: [0u8; 8],
        }
    }

    /// Render the system bar onto the buffer.
    ///
    /// `active_app_name`: title of the focused window, or "SlopOS" if none.
    /// `uptime_secs`: seconds since boot (from HPET or tick count).
    pub fn draw(
        &mut self,
        buf: &mut DrawBuffer,
        screen_width: u32,
        active_app_name: &str,
        uptime_secs: u64,
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

        // -- Background (semi-transparent) ------------------------------------
        gfx::fill_rect_blended_clipped(buf, 0, 0, sw, bar_h, BAR_BG, &clip);

        // -- Bottom border (1 px, opaque) -------------------------------------
        gfx::fill_rect_clipped(buf, 0, bar_h, sw, 1, PANEL_BORDER, &clip);

        // -- Left section -----------------------------------------------------
        let mut cursor_x = SYSTEM_BAR_PADDING_X;

        // System icon: filled green circle, 8 px diameter, centred vertically.
        let icon_cx = cursor_x + ICON_RADIUS;
        let icon_cy = bar_h / 2;
        gfx::draw_circle_filled(buf, icon_cx, icon_cy, ICON_RADIUS, SIGNAL_EXPAND);
        cursor_x += SYSTEM_BAR_ICON_SIZE + SYSTEM_BAR_ICON_GAP;

        // Active app name (or "SlopOS").
        let name = if active_app_name.is_empty() {
            "SlopOS"
        } else {
            active_app_name
        };
        let text_y = (bar_h - BAR_FONT_SIZE as i32) / 2;

        if let Some(font) = font {
            draw_name_ttf(buf, font, cursor_x, text_y, name, &clip);
        } else {
            draw_name_bitmap(buf, cursor_x, text_y, name, &clip);
        }

        // -- Right section (clock) --------------------------------------------
        let clock = format_clock(uptime_secs);
        self.last_clock = clock;

        let clock_str = core::str::from_utf8(&clock).unwrap_or("00:00:00");
        let clock_x = sw - SYSTEM_BAR_PADDING_X - clock_text_width(clock_str);
        let clock_y = text_y;

        gfx::draw_str_clipped(
            buf,
            clock_x,
            clock_y,
            clock_str,
            TEXT_PRIMARY,
            OPAQUE_BAR_BG,
            &clip,
        );
    }

    /// Returns true if (`px`, `py`) is inside the system bar region.
    pub fn hit_test(px: i32, py: i32) -> bool {
        let _ = px; // x is always valid (full-width bar)
        py >= 0 && py < SYSTEM_BAR_HEIGHT
    }

    /// Returns the damage rect for the clock area (right side) if the
    /// clock text changed since the last draw.
    pub fn clock_damage(&self, screen_width: u32) -> Option<DamageRect> {
        let clock = self.last_clock;
        // If the cached clock is all zeros, no draw has happened yet -- no
        // damage to report.
        if clock == [0u8; 8] {
            return None;
        }

        // The clock occupies the rightmost CLOCK_DAMAGE_WIDTH pixels of the
        // bar.  We always report this region so the compositor can repaint it
        // when the second ticks over.  The caller is responsible for comparing
        // the current uptime with the previous value; this method simply
        // exposes the geometry.
        let sw = screen_width as i32;
        Some(DamageRect {
            x0: (sw - CLOCK_DAMAGE_WIDTH).max(0),
            y0: 0,
            x1: sw - 1,
            y1: SYSTEM_BAR_HEIGHT, // include border row
        })
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Format `uptime_secs` into an 8-byte "HH:MM:SS" buffer without allocation.
fn format_clock(secs: u64) -> [u8; 8] {
    let h = (secs / 3600) % 100; // cap at 99 hours for display
    let m = (secs % 3600) / 60;
    let s = secs % 60;

    let mut buf = [0u8; 8];
    buf[0] = b'0' + (h / 10) as u8;
    buf[1] = b'0' + (h % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (m / 10) as u8;
    buf[4] = b'0' + (m % 10) as u8;
    buf[5] = b':';
    buf[6] = b'0' + (s / 10) as u8;
    buf[7] = b'0' + (s % 10) as u8;
    buf
}

/// Approximate pixel width of a clock string using the bitmap font.
fn clock_text_width(text: &str) -> i32 {
    gfx::font::string_width(text)
}

/// Draw the active app name using the TrueType font, truncating with "..."
/// if it exceeds `SYSTEM_BAR_MAX_APP_NAME_WIDTH`.
fn draw_name_ttf(
    buf: &mut DrawBuffer,
    font: &mut FontRenderer,
    x: i32,
    y: i32,
    name: &str,
    _clip: &DamageRect,
) {
    let max_w = SYSTEM_BAR_MAX_APP_NAME_WIDTH;
    let (full_w, _) = font.measure_text(name, BAR_FONT_SIZE);

    if full_w <= max_w {
        font.draw_text(buf, x, y, name, BAR_FONT_SIZE, TEXT_PRIMARY, OPAQUE_BAR_BG);
    } else {
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
            OPAQUE_BAR_BG,
        );
        let (pw, _) = font.measure_text(prefix, BAR_FONT_SIZE);
        font.draw_text(
            buf,
            x + pw,
            y,
            ELLIPSIS,
            BAR_FONT_SIZE,
            TEXT_PRIMARY,
            OPAQUE_BAR_BG,
        );
    }
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
/// "..." if it exceeds `SYSTEM_BAR_MAX_APP_NAME_WIDTH`.
fn draw_name_bitmap(buf: &mut DrawBuffer, x: i32, y: i32, name: &str, clip: &DamageRect) {
    let max_w = SYSTEM_BAR_MAX_APP_NAME_WIDTH;
    let full_w = gfx::font::string_width(name);

    if full_w <= max_w {
        gfx::draw_str_clipped(buf, x, y, name, TEXT_PRIMARY, OPAQUE_BAR_BG, clip);
    } else {
        let ell_w = gfx::font::string_width(ELLIPSIS);
        let budget = max_w - ell_w;
        let prefix_len = find_bitmap_prefix_len(name, budget);
        let prefix = &name[..prefix_len];

        gfx::draw_str_clipped(buf, x, y, prefix, TEXT_PRIMARY, OPAQUE_BAR_BG, clip);
        let pw = gfx::font::string_width(prefix);
        gfx::draw_str_clipped(buf, x + pw, y, ELLIPSIS, TEXT_PRIMARY, OPAQUE_BAR_BG, clip);
    }
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
