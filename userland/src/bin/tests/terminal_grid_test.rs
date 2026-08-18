#![feature(restricted_std)]

//! Terminal grid VT semantics — deferred autowrap and the editor's
//! region-based redraw contract.

use slopos_userland as _;

use slopos_abi::input::{MODIFIER_CTRL, MODIFIER_SHIFT};
use slopos_userland::apps::terminal::grid::TerminalGrid;
use slopos_userland::apps::terminal::input::{
    KeyAction, PointerState, Selection, collect_selection, encode_key, sanitize_paste,
    update_selection,
};

fn feed(g: &mut TerminalGrid, bytes: &[u8]) {
    for &b in bytes {
        g.process_byte(b);
    }
}

fn glyph(g: &TerminalGrid, row: usize, col: usize) -> char {
    char::from_u32(g.visible_cell(row, col).glyph()).unwrap_or('\u{FFFD}')
}

fn test_exactly_full_line_defers_wrap() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abc");
    if g.cursor_row != 0 || g.cursor_col != 2 {
        return false;
    }
    feed(&mut g, b"\r\nx");
    g.cursor_row == 1 && glyph(&g, 1, 0) == 'x' && glyph(&g, 0, 2) == 'c'
}

fn test_pending_wrap_commits_on_next_print() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abcx");
    glyph(&g, 1, 0) == 'x' && g.cursor_row == 1 && g.cursor_col == 1
}

fn test_carriage_return_cancels_pending_wrap() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abc\rX");
    glyph(&g, 0, 0) == 'X' && g.cursor_row == 0
}

fn test_sgr_preserves_pending_wrap() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abc\x1b[31md");
    glyph(&g, 1, 0) == 'd' && g.cursor_row == 1
}

/// The shell editor's wrapped-input redraw: move to the region start, erase
/// below, reprint.
fn test_editor_region_redraw_no_duplicate() -> bool {
    let mut g = TerminalGrid::new(6, 4);
    // First render: "$ abcde" = 7 cells over 4 cols -> 2 rows.
    feed(&mut g, b"$ abcde");
    if g.cursor_row != 1 {
        return false;
    }
    // Up to the region start, erase below, reprint the grown content.
    feed(&mut g, b"\r\x1b[1A\x1b[J$ abcdef");
    if glyph(&g, 0, 0) != '$' || glyph(&g, 0, 2) != 'a' {
        return false;
    }
    if glyph(&g, 1, 0) != 'c' || glyph(&g, 1, 3) != 'f' {
        return false;
    }
    glyph(&g, 2, 0) == ' '
}

fn test_backspace_cancels_pending_wrap() -> bool {
    let mut g = TerminalGrid::new(5, 3);
    feed(&mut g, b"abc\x08X");
    glyph(&g, 0, 1) == 'X' && g.cursor_row == 0 && g.cursor_col == 2
}

/// DECSET 1049 swaps to a blank alt screen; DECRST restores the main
/// screen's content and cursor.
fn test_alt_screen_saves_and_restores_main() -> bool {
    let mut g = TerminalGrid::new(3, 10);
    feed(&mut g, b"main\x1b[2;3H");
    feed(&mut g, b"\x1b[?1049h");
    if glyph(&g, 0, 0) != ' ' || g.cursor_row != 0 || g.cursor_col != 0 {
        return false;
    }
    feed(&mut g, b"ALT");
    if glyph(&g, 0, 0) != 'A' {
        return false;
    }
    feed(&mut g, b"\x1b[?1049l");
    glyph(&g, 0, 0) == 'm' && glyph(&g, 0, 3) == 'n' && g.cursor_row == 1 && g.cursor_col == 2
}

fn test_alt_screen_does_not_feed_scrollback() -> bool {
    let mut g = TerminalGrid::new(2, 4);
    feed(&mut g, b"\x1b[?1049hA1\r\nA2\r\nA3\x1b[?1049l");
    g.scroll_view_up(1);
    !g.viewing_history()
}

fn test_bracketed_paste_tracks_decset_2004() -> bool {
    let mut g = TerminalGrid::new(3, 10);
    if g.bracketed_paste() {
        return false;
    }
    feed(&mut g, b"\x1b[?2004h");
    if !g.bracketed_paste() {
        return false;
    }
    feed(&mut g, b"\x1b[?2004l");
    !g.bracketed_paste()
}

/// Ctrl+Shift+C is the clipboard-copy chord; plain Ctrl+C still reaches the
/// line discipline.
fn test_ctrl_shift_c_copies_not_sigint() -> bool {
    if !matches!(
        encode_key(0x03, 0x2E, 0x03, MODIFIER_CTRL | MODIFIER_SHIFT),
        KeyAction::CopySelection
    ) {
        return false;
    }
    match encode_key(0x03, 0x2E, 0x03, MODIFIER_CTRL) {
        KeyAction::ToMaster(b) => b.as_bytes() == [0x03],
        _ => false,
    }
}

/// Ctrl+Shift+V requests a compositor paste; plain Ctrl+V passes through.
fn test_ctrl_shift_v_requests_paste() -> bool {
    if !matches!(
        encode_key(0x16, 0x2F, 0x16, MODIFIER_CTRL | MODIFIER_SHIFT),
        KeyAction::RequestPaste
    ) {
        return false;
    }
    match encode_key(0x16, 0x2F, 0x16, MODIFIER_CTRL) {
        KeyAction::ToMaster(b) => b.as_bytes() == [0x16],
        _ => false,
    }
}

/// A payload closing the paste bracket to inject keystrokes (the xterm
/// CVE-2022-45063 class), including the splice where stripping an inner
/// marker rejoins an outer ESC with a trailing `[201~`.
fn test_paste_cannot_inject_bracket_end_marker() -> bool {
    let mut out = [0u8; 64];
    let n = sanitize_paste(b"safe\x1b[201~rm -rf /\r", &mut out);
    if &out[..n] != b"safe[201~rm -rf /\r" {
        return false;
    }
    let n = sanitize_paste(b"\x1b\x1b[201~[201~x", &mut out);
    if &out[..n] != b"[201~[201~x" {
        return false;
    }
    !out[..n].windows(6).any(|w| w == b"\x1b[201~")
}

/// Paste types like a keyboard: newlines become Enter (\r), control bytes
/// (Ctrl+C, ESC, DEL) are dropped.
fn test_paste_types_like_keys() -> bool {
    let mut out = [0u8; 64];
    let n = sanitize_paste(b"one\r\ntwo\nthree", &mut out);
    if &out[..n] != b"one\rtwo\rthree" {
        return false;
    }
    let n = sanitize_paste(b"a\x03b\x1b[Ac\x7fd\te", &mut out);
    &out[..n] == b"ab[Acd\te"
}

/// The selection is content-anchored, so it still yields the selected text
/// after output scrolls it off the live region into scrollback.
fn test_selection_survives_scroll() -> bool {
    const CW: i32 = 8;
    const CH: i32 = 16;
    let mut g = TerminalGrid::new(5, 10);
    feed(&mut g, b"ANCHORED\r\nsecond\r\nthird");

    // Drag-select "ANCHORED" on screen row 0.
    let mut sel = Selection::NONE;
    let mut ptr = PointerState::new();
    ptr.has_focus = true;
    ptr.button_state = 0x01; // MOUSE_LEFT
    ptr.last_x = 0;
    ptr.last_y = 0;
    update_selection(&mut ptr, &mut sel, &g, CW, CH);
    ptr.last_x = 8 * CW;
    ptr.last_y = 0;
    update_selection(&mut ptr, &mut sel, &g, CW, CH);
    ptr.button_state = 0;
    update_selection(&mut ptr, &mut sel, &g, CW, CH);

    let mut buf = [0u8; 64];
    let n = collect_selection(&g, &sel, &mut buf);
    if &buf[..n] != b"ANCHORED" {
        return false;
    }

    for _ in 0..8 {
        feed(&mut g, b"flood\r\n");
    }
    let n = collect_selection(&g, &sel, &mut buf);
    &buf[..n] == b"ANCHORED"
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("selection_survives_scroll", test_selection_survives_scroll),
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
        (
            "alt_screen_saves_and_restores_main",
            test_alt_screen_saves_and_restores_main,
        ),
        (
            "alt_screen_does_not_feed_scrollback",
            test_alt_screen_does_not_feed_scrollback,
        ),
        (
            "bracketed_paste_tracks_decset_2004",
            test_bracketed_paste_tracks_decset_2004,
        ),
        (
            "ctrl_shift_c_copies_not_sigint",
            test_ctrl_shift_c_copies_not_sigint,
        ),
        (
            "ctrl_shift_v_requests_paste",
            test_ctrl_shift_v_requests_paste,
        ),
        (
            "paste_cannot_inject_bracket_end_marker",
            test_paste_cannot_inject_bracket_end_marker,
        ),
        ("paste_types_like_keys", test_paste_types_like_keys),
    ]);
}
