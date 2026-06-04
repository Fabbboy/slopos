//! Glyph rendering for the terminal grid.
//!
//! Paints the visible `TerminalGrid` into the surface's `DrawBuffer`: every
//! cell is blitted via the per-app `GlyphAtlas` (the `crate::gfx::font`
//! OnceLock), the cursor cell is drawn in inverse video when visible (subject
//! to the blink phase), and any pointer selection is highlighted.

use slopos_abi::draw::Color32;

use crate::gfx::DrawBuffer;
use crate::gfx::font;

use super::grid::{Cell, TerminalGrid};
use super::input::{Selection, cell_in_selection};
use super::surface;

/// Default window background (matches the grid's default cell background).
const WINDOW_BG: Color32 = Color32::rgb(0x1E, 0x1E, 0x1E);
/// Selection highlight background (matches the shell's selection blue).
const SELECTION_BG: Color32 = Color32::rgb(0x26, 0x4F, 0x78);

#[inline]
fn cell_width() -> i32 {
    font::cell_width()
}

#[inline]
fn cell_height() -> i32 {
    font::cell_height()
}

/// Convert a grid color (`0x00RRGGBB`) to an opaque `Color32`.
#[inline]
fn color_from_rgb(rgb: u32) -> Color32 {
    Color32(0xFF00_0000 | (rgb & 0x00FF_FFFF))
}

/// Draw one cell at grid (row, col) with the given fg/bg overrides.
fn draw_cell_at(
    buf: &mut DrawBuffer,
    row: usize,
    col: usize,
    cell: &Cell,
    fg: Color32,
    bg: Color32,
) {
    let x = col as i32 * cell_width();
    let y = row as i32 * cell_height();
    font::draw_glyph(buf, x, y, cell.glyph(), fg, bg);
}

/// Full redraw of the grid + cursor + selection into the surface frame, then
/// present. Called once per event-loop wake when the grid is dirty.
pub fn render(grid: &TerminalGrid, selection: &Selection, cursor_on: bool) {
    let rows = grid.rows as usize;
    let cols = grid.cols as usize;

    surface::draw(|buf| {
        // Clear to the window background first so partial-cell remainder pixels
        // (when pixel dims aren't a clean multiple of the cell size) match the theme.
        let w = buf.width() as i32;
        let h = buf.height() as i32;
        crate::gfx::fill_rect(buf, 0, 0, w, h, WINDOW_BG);

        let sel_range = selection.ordered();

        for row in 0..rows {
            let abs = grid.screen_to_abs(row);
            for col in 0..cols {
                let cell = grid.visible_cell(row, col);
                let mut fg = color_from_rgb(cell.attrs.fg);
                let mut bg = color_from_rgb(cell.attrs.bg);

                // Selection highlight. Content-anchored, so it tracks the
                // selected text through scrollback (unlike the cursor, which
                // is suppressed while viewing history).
                if let Some((lo, hi)) = sel_range {
                    if cell_in_selection(abs, col, lo, hi) {
                        bg = SELECTION_BG;
                    }
                }

                draw_cell_at(buf, row, col, &cell, fg, bg);

                // Cursor: inverse video on the cursor cell when blinking on.
                let is_cursor = !grid.viewing_history()
                    && grid.cursor_visible
                    && cursor_on
                    && row == grid.cursor_row as usize
                    && col == grid.cursor_col as usize;
                if is_cursor {
                    core::mem::swap(&mut fg, &mut bg);
                    draw_cell_at(buf, row, col, &cell, fg, bg);
                }
            }
        }
    });
    surface::present();
}
