#![feature(restricted_std)]

//! Terminal grid VT semantics — deferred autowrap and the editor's
//! region-based redraw contract.
//!
//! Regression target: the grid wrapped eagerly when a char filled the last
//! column, so an exactly-full line followed by `\r\n` advanced TWO rows and
//! the shell editor's redraw left a stale duplicate of the previous render
//! on screen.

use slopos_userland as _;

use slopos_userland::apps::terminal::grid::TerminalGrid;

fn feed(g: &mut TerminalGrid, bytes: &[u8]) {
    for &b in bytes {
        g.process_byte(b);
    }
}

fn glyph(g: &TerminalGrid, row: usize, col: usize) -> char {
    char::from_u32(g.visible_cell(row, col).glyph()).unwrap_or('\u{FFFD}')
}

/// A char in the last column leaves the cursor resting there; `\r\n`
/// afterwards advances exactly one row.
fn test_exactly_full_line_defers_wrap() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abc");
    if g.cursor_row != 0 || g.cursor_col != 2 {
        return false;
    }
    feed(&mut g, b"\r\nx");
    g.cursor_row == 1 && glyph(&g, 1, 0) == 'x' && glyph(&g, 0, 2) == 'c'
}

/// The pending wrap commits when the next printable char arrives.
fn test_pending_wrap_commits_on_next_print() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abcx");
    glyph(&g, 1, 0) == 'x' && g.cursor_row == 1 && g.cursor_col == 1
}

/// Carriage return cancels the pending wrap (overwrite-in-place).
fn test_carriage_return_cancels_pending_wrap() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abc\rX");
    glyph(&g, 0, 0) == 'X' && g.cursor_row == 0
}

/// SGR between the edge char and the next print must not eat the wrap.
fn test_sgr_preserves_pending_wrap() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abc\x1b[31md");
    glyph(&g, 1, 0) == 'd' && g.cursor_row == 1
}

/// The shell editor's wrapped-input redraw: move to the region start, erase
/// below, reprint. The grid must end up with exactly one copy of the
/// content — no stale duplicate rows.
fn test_editor_region_redraw_no_duplicate() -> bool {
    let mut g = TerminalGrid::new(6, 4);
    // First render: "$ abcde" = 7 cells over 4 cols -> 2 rows.
    feed(&mut g, b"$ abcde");
    if g.cursor_row != 1 {
        return false;
    }
    // Editor redraw with one more char typed: up to the region start,
    // erase below, reprint the grown content.
    feed(&mut g, b"\r\x1b[1A\x1b[J$ abcdef");
    if glyph(&g, 0, 0) != '$' || glyph(&g, 0, 2) != 'a' {
        return false;
    }
    if glyph(&g, 1, 0) != 'c' || glyph(&g, 1, 3) != 'f' {
        return false;
    }
    // The old second row was erased before the reprint — nothing stale
    // beyond the new content.
    glyph(&g, 2, 0) == ' '
}

/// Backspace from the pending-wrap position moves within the same row.
fn test_backspace_cancels_pending_wrap() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abc\x08X");
    glyph(&g, 0, 1) == 'X' && g.cursor_row == 0 && g.cursor_col == 2
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "exactly_full_line_defers_wrap",
            test_exactly_full_line_defers_wrap,
        ),
        (
            "pending_wrap_commits_on_next_print",
            test_pending_wrap_commits_on_next_print,
        ),
        (
            "carriage_return_cancels_pending_wrap",
            test_carriage_return_cancels_pending_wrap,
        ),
        (
            "sgr_preserves_pending_wrap",
            test_sgr_preserves_pending_wrap,
        ),
        (
            "editor_region_redraw_no_duplicate",
            test_editor_region_redraw_no_duplicate,
        ),
        (
            "backspace_cancels_pending_wrap",
            test_backspace_cancels_pending_wrap,
        ),
    ]);
}
