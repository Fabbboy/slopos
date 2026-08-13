//! Glyph rendering for the terminal grid.
//!
//! Paints the visible `TerminalGrid` into the surface's `DrawBuffer`: every
//! cell is blitted via the per-app `GlyphAtlas` (the `crate::gfx::font`
//! OnceLock), the cursor cell is drawn in inverse video when visible (subject
//! to the blink phase), and any pointer selection is highlighted.
//!
//! Only cells the grid marked damaged are repainted, and the presented damage
//! rect is their bounding box. A cursor blink therefore moves one cell's worth
//! of pixels through the compositor, not a window's.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::Color32;

use crate::gfx::DrawBuffer;
use crate::gfx::font;

use super::grid::{Cell, TerminalGrid};
use super::input::{Selection, cell_in_selection};
use super::surface;

use slopos_terminal_core::damage::CellDamage;

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

/// The union of `damage`'s spans as a pixel rect, or `None` when nothing is
/// damaged. Rows below the last grid row are not included, so the trailing
/// partial-cell remainder is only repainted by a full paint.
fn damage_bounds(damage: &CellDamage) -> Option<DamageRect> {
    let cw = cell_width();
    let ch = cell_height();
    let mut out: Option<DamageRect> = None;
    for row in 0..damage.rows() {
        let Some(span) = damage.span(row) else {
            continue;
        };
        let rect = DamageRect {
            x0: span.first as i32 * cw,
            y0: row as i32 * ch,
            x1: (span.last as i32 + 1) * cw - 1,
            y1: (row as i32 + 1) * ch - 1,
        };
        out = Some(match out {
            Some(acc) => acc.union(&rect),
            None => rect,
        });
    }
    out
}

/// Paint the damaged cells of `grid` and present exactly that region.
///
/// `damage` is consumed: a repaint that is dropped rather than presented would
/// leave the compositor showing stale cells forever, since the grid has already
/// forgotten they changed.
pub fn render_damage(
    grid: &TerminalGrid,
    selection: &Selection,
    cursor_on: bool,
    damage: &CellDamage,
) {
    let Some(bounds) = damage_bounds(damage) else {
        return;
    };

    surface::draw(|buf| {
        // Confine every primitive to the damaged region: a glyph's cell is the
        // atlas cell size, which need not equal the grid's, and an overhanging
        // blit would dirty pixels outside the rect being presented.
        buf.set_scissor(Some(bounds));
        for row in 0..damage.rows() {
            let Some(span) = damage.span(row) else {
                continue;
            };
            draw_row_span(buf, grid, selection, cursor_on, row, span.first, span.last);
        }
        buf.set_scissor(None);
    });

    surface::present_region(
        bounds.x0,
        bounds.y0,
        bounds.x1 - bounds.x0 + 1,
        bounds.y1 - bounds.y0 + 1,
    );
}

/// Repaint every cell and present the whole surface. Used for the first frame
/// and after a resize, where the backing buffer's contents are undefined and no
/// per-cell damage set describes what has to be written.
pub fn render_full(grid: &TerminalGrid, selection: &Selection, cursor_on: bool) {
    let rows = grid.rows as usize;
    let cols = grid.cols as usize;

    surface::draw(|buf| {
        // Clear to the window background first so partial-cell remainder pixels
        // (when pixel dims aren't a clean multiple of the cell size) match the theme.
        let w = buf.width() as i32;
        let h = buf.height() as i32;
        crate::gfx::fill_rect(buf, 0, 0, w, h, WINDOW_BG);

        if cols > 0 {
            for row in 0..rows {
                draw_row_span(buf, grid, selection, cursor_on, row, 0, cols as u16 - 1);
            }
        }
    });
    surface::present();
}

/// Paint cells `first..=last` of one screen row.
fn draw_row_span(
    buf: &mut DrawBuffer,
    grid: &TerminalGrid,
    selection: &Selection,
    cursor_on: bool,
    row: usize,
    first: u16,
    last: u16,
) {
    let cols = grid.cols as usize;
    if row >= grid.rows as usize || cols == 0 {
        return;
    }
    let sel_range = selection.ordered();
    let abs = grid.screen_to_abs(row);
    let last = (last as usize).min(cols - 1);

    for col in (first as usize)..=last {
        let cell = grid.visible_cell(row, col);
        let mut fg = color_from_rgb(cell.attrs.fg);
        let mut bg = color_from_rgb(cell.attrs.bg);

        // Selection highlight. Content-anchored, so it tracks the selected text
        // through scrollback (unlike the cursor, which is suppressed while
        // viewing history).
        if let Some((lo, hi)) = sel_range {
            if cell_in_selection(abs, col, lo, hi) {
                bg = SELECTION_BG;
            }
        }

        // Cursor: inverse video on the cursor cell when blinking on.
        let is_cursor = !grid.viewing_history()
            && grid.cursor_visible
            && cursor_on
            && row == grid.cursor_row as usize
            && col == grid.cursor_col as usize;
        if is_cursor {
            core::mem::swap(&mut fg, &mut bg);
        }

        draw_cell_at(buf, row, col, &cell, fg, bg);
    }
}
