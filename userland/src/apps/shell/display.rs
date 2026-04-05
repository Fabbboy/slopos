//! Display state, scrollback buffer, and rendering functions.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicUsize, Ordering};

use slopos_abi::draw::Color32;

use crate::gfx::font;
use crate::gfx::{self, DrawBuffer};
use crate::syscall::fs;

use super::surface;

pub const SHELL_BG_COLOR: Color32 = Color32::rgb(0x1E, 0x1E, 0x1E);
pub const SHELL_FG_COLOR: Color32 = Color32::rgb(0xE6, 0xE6, 0xE6);

pub const COLOR_DEFAULT: u8 = 0;
pub const COLOR_DIR_BLUE: u8 = 1;
pub const COLOR_EXEC_GREEN: u8 = 2;
pub const COLOR_ERROR_RED: u8 = 3;
pub const COLOR_WARN_YELLOW: u8 = 4;
pub const COLOR_PROMPT_ACCENT: u8 = 5;
pub const COLOR_COMMENT_GRAY: u8 = 6;
pub const COLOR_PATH_BLUE: u8 = 7;
pub const COLOR_SELECTION_BG: u8 = 8;

pub const PALETTE_SIZE: usize = 16;

/// 4-bit indexed color palette for per-character foreground colors in scrollback.
pub static PALETTE: [Color32; PALETTE_SIZE] = [
    SHELL_FG_COLOR,                 // 0: default
    Color32::rgb(0x5C, 0x9E, 0xD6), // 1: directory blue
    Color32::rgb(0x98, 0xC3, 0x79), // 2: executable green
    Color32::rgb(0xE0, 0x6C, 0x75), // 3: error red
    Color32::rgb(0xE5, 0xC0, 0x7B), // 4: warning yellow
    Color32::rgb(0xC6, 0x78, 0xDD), // 5: prompt accent
    Color32::rgb(0x5C, 0x63, 0x70), // 6: comment gray
    Color32::rgb(0x61, 0xAF, 0xEF), // 7: path blue
    Color32::rgb(0x26, 0x4F, 0x78), // 8: selection background
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
];

pub const SHELL_WINDOW_WIDTH: i32 = 640;
pub const SHELL_WINDOW_HEIGHT: i32 = 480;
pub const SHELL_TAB_WIDTH: i32 = 4;
pub const SHELL_SCROLLBACK_LINES: usize = 256;
pub const SHELL_SCROLLBACK_COLS: usize = 160;

// =============================================================================
// DisplayState: Cell-based state (no borrow conflicts)
// =============================================================================

pub struct DisplayState {
    pub width: AtomicI32,
    pub height: AtomicI32,
    pub pitch: AtomicUsize,
    pub bytes_pp: AtomicU8,
    pub cols: AtomicI32,
    pub rows: AtomicI32,
    pub cursor_col: AtomicI32,
    pub cursor_line: AtomicI32,
    pub origin: AtomicI32,
    pub total_lines: AtomicI32,
    pub view_top: AtomicI32,
    pub follow: AtomicBool,
    pub fg: AtomicU32,
    pub bg: AtomicU32,
}

#[inline]
fn load_color(a: &AtomicU32) -> Color32 {
    Color32(a.load(Ordering::Relaxed))
}

#[inline]
fn store_color(a: &AtomicU32, c: Color32) {
    a.store(c.to_u32(), Ordering::Relaxed);
}

impl DisplayState {
    pub const fn new() -> Self {
        Self {
            width: AtomicI32::new(0),
            height: AtomicI32::new(0),
            pitch: AtomicUsize::new(0),
            bytes_pp: AtomicU8::new(4),
            cols: AtomicI32::new(0),
            rows: AtomicI32::new(0),
            cursor_col: AtomicI32::new(0),
            cursor_line: AtomicI32::new(0),
            origin: AtomicI32::new(0),
            total_lines: AtomicI32::new(1),
            view_top: AtomicI32::new(0),
            follow: AtomicBool::new(true),
            fg: AtomicU32::new(SHELL_FG_COLOR.to_u32()),
            bg: AtomicU32::new(SHELL_BG_COLOR.to_u32()),
        }
    }

    pub fn line_slot(&self, logical: i32) -> usize {
        let max_lines = SHELL_SCROLLBACK_LINES as i32;
        ((self.origin.load(Ordering::Relaxed) + logical).rem_euclid(max_lines)) as usize
    }

    pub fn cursor(&self) -> (i32, i32) {
        let row = (self.cursor_line.load(Ordering::Relaxed)
            - self.view_top.load(Ordering::Relaxed))
        .clamp(0, self.rows.load(Ordering::Relaxed).saturating_sub(1));
        (self.cursor_col.load(Ordering::Relaxed), row)
    }

    pub fn reset(&self) {
        self.cursor_col.store(0, Ordering::Relaxed);
        self.cursor_line.store(0, Ordering::Relaxed);
        self.origin.store(0, Ordering::Relaxed);
        self.total_lines.store(1, Ordering::Relaxed);
        self.view_top.store(0, Ordering::Relaxed);
        self.follow.store(true, Ordering::Relaxed);
    }
}

pub static DISPLAY: DisplayState = DisplayState::new();
static OUTPUT_FD: AtomicI32 = AtomicI32::new(-1);
static CURRENT_COLOR_IDX: AtomicU8 = AtomicU8::new(0);

#[inline]
fn current_color_idx() -> u8 {
    CURRENT_COLOR_IDX.load(Ordering::Relaxed)
}

#[inline]
fn set_current_color_idx(idx: u8) {
    CURRENT_COLOR_IDX.store(idx, Ordering::Relaxed);
}

fn palette_index_for(color: Color32) -> u8 {
    let target = color.to_u32();
    for (i, c) in PALETTE.iter().enumerate() {
        if c.to_u32() == target {
            return i as u8;
        }
    }
    COLOR_DEFAULT
}

// =============================================================================
// Scrollback module: safe accessors for large arrays
// =============================================================================

pub mod scrollback {
    use super::*;

    struct Scrollback {
        data: [u8; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS],
        colors: [u8; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS],
        lens: [u16; SHELL_SCROLLBACK_LINES],
    }

    static SCROLLBACK: Mutex<Scrollback> = Mutex::new(Scrollback {
        data: [0; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS],
        colors: [0; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS],
        lens: [0; SHELL_SCROLLBACK_LINES],
    });

    #[inline]
    pub fn get_line_len(slot: usize) -> u16 {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        SCROLLBACK.lock().unwrap().lens[slot]
    }

    #[inline]
    pub fn set_line_len(slot: usize, len: u16) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        SCROLLBACK.lock().unwrap().lens[slot] = len;
    }

    #[inline]
    pub fn set_char(slot: usize, col: usize, ch: u8) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        SCROLLBACK.lock().unwrap().data[slot * SHELL_SCROLLBACK_COLS + col] = ch;
    }

    #[inline]
    pub fn get_char(slot: usize, col: usize) -> u8 {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        SCROLLBACK.lock().unwrap().data[slot * SHELL_SCROLLBACK_COLS + col]
    }

    #[inline]
    pub fn set_color(slot: usize, col: usize, color_idx: u8) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        SCROLLBACK.lock().unwrap().colors[slot * SHELL_SCROLLBACK_COLS + col] = color_idx;
    }

    #[inline]
    pub fn get_color(slot: usize, col: usize) -> u8 {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        SCROLLBACK.lock().unwrap().colors[slot * SHELL_SCROLLBACK_COLS + col]
    }

    pub fn clear_line(slot: usize) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let mut sb = SCROLLBACK.lock().unwrap();
        let start = slot * SHELL_SCROLLBACK_COLS;
        for i in start..start + SHELL_SCROLLBACK_COLS {
            sb.data[i] = 0;
            sb.colors[i] = 0;
        }
        sb.lens[slot] = 0;
    }

    pub fn clear_all() {
        let mut sb = SCROLLBACK.lock().unwrap();
        for byte in sb.data.iter_mut() {
            *byte = 0;
        }
        for c in sb.colors.iter_mut() {
            *c = 0;
        }
        for len in sb.lens.iter_mut() {
            *len = 0;
        }
    }

    pub fn write_line(slot: usize, content: &[u8]) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let len = content.len().min(SHELL_SCROLLBACK_COLS);
        let mut sb = SCROLLBACK.lock().unwrap();
        let start = slot * SHELL_SCROLLBACK_COLS;
        for i in start..start + SHELL_SCROLLBACK_COLS {
            sb.data[i] = 0;
            sb.colors[i] = 0;
        }
        for (i, &b) in content.iter().take(len).enumerate() {
            sb.data[start + i] = b;
        }
        sb.lens[slot] = len as u16;
    }

    pub fn write_line_colored(slot: usize, content: &[u8], color_indices: &[u8]) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let len = content.len().min(SHELL_SCROLLBACK_COLS);
        let mut sb = SCROLLBACK.lock().unwrap();
        let start = slot * SHELL_SCROLLBACK_COLS;
        for i in start..start + SHELL_SCROLLBACK_COLS {
            sb.data[i] = 0;
            sb.colors[i] = 0;
        }
        for (i, &b) in content.iter().take(len).enumerate() {
            sb.data[start + i] = b;
            if i < color_indices.len() {
                sb.colors[start + i] = color_indices[i];
            }
        }
        sb.lens[slot] = len as u16;
    }

    /// Snapshot a row's character data, color data, and length in a single lock.
    ///
    /// Used by `draw_row_from_scrollback` to avoid re-entrant locking (Mutex is
    /// not re-entrant, so calling `get_color` inside `with_line` would deadlock).
    pub fn row_snapshot(
        slot: usize,
    ) -> (
        u16,
        [u8; SHELL_SCROLLBACK_COLS],
        [u8; SHELL_SCROLLBACK_COLS],
    ) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let sb = SCROLLBACK.lock().unwrap();
        let start = slot * SHELL_SCROLLBACK_COLS;
        let mut chars = [0u8; SHELL_SCROLLBACK_COLS];
        let mut colors = [0u8; SHELL_SCROLLBACK_COLS];
        chars.copy_from_slice(&sb.data[start..start + SHELL_SCROLLBACK_COLS]);
        colors.copy_from_slice(&sb.colors[start..start + SHELL_SCROLLBACK_COLS]);
        (sb.lens[slot], chars, colors)
    }
}

// =============================================================================
// Free drawing functions (no &mut self, explicit parameters)
// =============================================================================

fn draw_char_at(buf: &mut DrawBuffer, col: i32, row: i32, c: u8, fg: Color32, bg: Color32) {
    let x = col * font::cell_width();
    let y = row * font::cell_height();
    gfx::font::draw_char(buf, x, y, c, fg, bg);
}

fn clear_row(buf: &mut DrawBuffer, row: i32, width: i32, bg: Color32) {
    gfx::fill_rect(
        buf,
        0,
        row * font::cell_height(),
        width,
        font::cell_height(),
        bg,
    );
}

fn draw_row_from_scrollback(buf: &mut DrawBuffer, display: &DisplayState, logical: i32, row: i32) {
    let bg = load_color(&display.bg);
    let width = display.width.load(Ordering::Relaxed);
    let cols = display.cols.load(Ordering::Relaxed);
    let total_lines = display.total_lines.load(Ordering::Relaxed);

    clear_row(buf, row, width, bg);

    if logical < 0 || logical >= total_lines {
        return;
    }

    let slot = display.line_slot(logical);
    // Snapshot the entire row in one lock to avoid re-entrant Mutex deadlock.
    let (len, chars, colors) = scrollback::row_snapshot(slot);
    let draw_len = (len as usize).min(cols as usize);

    if draw_len == 0 {
        return;
    }

    for col in 0..draw_len {
        let ch = chars[col];
        if ch != 0 {
            let fg = PALETTE[colors[col] as usize % PALETTE_SIZE];
            draw_char_at(buf, col as i32, row, ch, fg, bg);
        }
    }
}

fn redraw_view(buf: &mut DrawBuffer, display: &DisplayState) {
    let bg = load_color(&display.bg);
    let width = display.width.load(Ordering::Relaxed);
    let height = display.height.load(Ordering::Relaxed);
    let rows = display.rows.load(Ordering::Relaxed);
    let view_top = display.view_top.load(Ordering::Relaxed);

    // Clear entire view
    gfx::fill_rect(buf, 0, 0, width, height, bg);

    // Draw each row
    for row in 0..rows {
        draw_row_from_scrollback(buf, display, view_top + row, row);
    }
}

fn scroll_up_fast(buf: &mut DrawBuffer, display: &DisplayState) -> bool {
    let width = display.width.load(Ordering::Relaxed);
    let height = display.height.load(Ordering::Relaxed);
    let bg = load_color(&display.bg);

    if height <= font::cell_height() {
        return false;
    }

    buf.blit(
        0,
        font::cell_height(),
        0,
        0,
        width,
        height - font::cell_height(),
    );

    gfx::fill_rect(
        buf,
        0,
        height - font::cell_height(),
        width,
        font::cell_height(),
        bg,
    );

    true
}

// =============================================================================
// State update functions (no drawing)
// =============================================================================

fn update_new_line(display: &DisplayState) {
    display.cursor_col.store(0, Ordering::Relaxed);
    let cursor_line = display.cursor_line.load(Ordering::Relaxed) + 1;
    display.cursor_line.store(cursor_line, Ordering::Relaxed);

    let total_lines = display.total_lines.load(Ordering::Relaxed);
    if cursor_line >= total_lines {
        if total_lines < SHELL_SCROLLBACK_LINES as i32 {
            display
                .total_lines
                .store(total_lines + 1, Ordering::Relaxed);
        } else {
            let origin =
                (display.origin.load(Ordering::Relaxed) + 1) % SHELL_SCROLLBACK_LINES as i32;
            display.origin.store(origin, Ordering::Relaxed);
            display
                .cursor_line
                .store(total_lines - 1, Ordering::Relaxed);
            let view_top = display.view_top.load(Ordering::Relaxed);
            if view_top > 0 {
                display.view_top.store(view_top - 1, Ordering::Relaxed);
            }
        }
        let slot = display.line_slot(display.cursor_line.load(Ordering::Relaxed));
        scrollback::clear_line(slot);
    }
}

fn update_char_state(display: &DisplayState, c: u8) {
    let cursor_col = display.cursor_col.load(Ordering::Relaxed);
    let cursor_line = display.cursor_line.load(Ordering::Relaxed);
    let cols = display.cols.load(Ordering::Relaxed);
    let slot = display.line_slot(cursor_line);

    if (cursor_col as usize) < SHELL_SCROLLBACK_COLS {
        scrollback::set_char(slot, cursor_col as usize, c);
        scrollback::set_color(slot, cursor_col as usize, current_color_idx());
        let len = scrollback::get_line_len(slot) as i32;
        if cursor_col + 1 > len {
            scrollback::set_line_len(slot, (cursor_col + 1) as u16);
        }
    }

    display.cursor_col.store(cursor_col + 1, Ordering::Relaxed);
    if display.cursor_col.load(Ordering::Relaxed) >= cols {
        update_new_line(display);
    }
}

fn update_backspace_state(display: &DisplayState) {
    let mut cursor_col = display.cursor_col.load(Ordering::Relaxed);
    let mut cursor_line = display.cursor_line.load(Ordering::Relaxed);

    if cursor_col > 0 {
        cursor_col -= 1;
    } else if cursor_line > 0 {
        cursor_line -= 1;
        let slot = display.line_slot(cursor_line);
        let len = scrollback::get_line_len(slot) as i32;
        cursor_col = if len > 0 {
            (len - 1).clamp(0, display.cols.load(Ordering::Relaxed).saturating_sub(1))
        } else {
            0
        };
    } else {
        return;
    }

    display.cursor_col.store(cursor_col, Ordering::Relaxed);
    display.cursor_line.store(cursor_line, Ordering::Relaxed);

    let slot = display.line_slot(cursor_line);
    if (cursor_col as usize) < SHELL_SCROLLBACK_COLS {
        scrollback::set_char(slot, cursor_col as usize, 0);
        scrollback::set_color(slot, cursor_col as usize, 0);
        let mut len = scrollback::get_line_len(slot) as i32;
        while len > 0 {
            if scrollback::get_char(slot, (len - 1) as usize) != 0 {
                break;
            }
            len -= 1;
        }
        scrollback::set_line_len(slot, len as u16);
    }
}

// =============================================================================
// Console operations (combined state + render)
// =============================================================================

fn console_write(display: &DisplayState, text: &[u8]) {
    let follow = display.follow.load(Ordering::Relaxed);
    let mut needs_scroll = false;
    let old_view_top = display.view_top.load(Ordering::Relaxed);
    let start_line = display.cursor_line.load(Ordering::Relaxed); // Track first line modified

    // Phase 1: Update state
    for &b in text {
        match b {
            b'\n' => {
                let old_total = display.total_lines.load(Ordering::Relaxed);
                update_new_line(display);

                // Check if we need to scroll view
                if follow {
                    let rows = display.rows.load(Ordering::Relaxed);
                    let total = display.total_lines.load(Ordering::Relaxed);
                    let max_top = (total - rows).max(0);
                    if display.view_top.load(Ordering::Relaxed) != max_top {
                        display.view_top.store(max_top, Ordering::Relaxed);
                        if total > old_total
                            || display.view_top.load(Ordering::Relaxed) != old_view_top
                        {
                            needs_scroll = true;
                        }
                    }
                }
            }
            b'\r' => display.cursor_col.store(0, Ordering::Relaxed),
            b'\t' => {
                for _ in 0..SHELL_TAB_WIDTH {
                    update_char_state(display, b' ');
                }
            }
            b'\x08' => update_backspace_state(display),
            0x20..=0x7E => update_char_state(display, b),
            _ => {}
        }
    }

    // Phase 2: Render if following
    if follow {
        surface::draw(|buf| {
            let view_top = display.view_top.load(Ordering::Relaxed);
            let rows = display.rows.load(Ordering::Relaxed);
            let cursor_line = display.cursor_line.load(Ordering::Relaxed);

            if needs_scroll {
                let view_diff = display.view_top.load(Ordering::Relaxed) - old_view_top;
                if view_diff == 1 && scroll_up_fast(buf, display) {
                    for line in start_line..=cursor_line {
                        let row = line - view_top;
                        if row >= 0 && row < rows {
                            draw_row_from_scrollback(buf, display, line, row);
                        }
                    }
                } else {
                    redraw_view(buf, display);
                }
            } else {
                for line in start_line..=cursor_line {
                    let row = line - view_top;
                    if row >= 0 && row < rows {
                        draw_row_from_scrollback(buf, display, line, row);
                    }
                }
            }
        });
    }
}

fn console_clear(display: &DisplayState) {
    display.reset();
    scrollback::clear_all();

    surface::draw(|buf| {
        let bg = load_color(&display.bg);
        let width = display.width.load(Ordering::Relaxed);
        let height = display.height.load(Ordering::Relaxed);
        gfx::fill_rect(buf, 0, 0, width, height, bg);
    });
}

fn scroll_view(display: &DisplayState, delta: i32) {
    let rows = display.rows.load(Ordering::Relaxed);
    let total_lines = display.total_lines.load(Ordering::Relaxed);

    if total_lines <= rows {
        return;
    }

    let max_top = (total_lines - rows).max(0);
    let current = display.view_top.load(Ordering::Relaxed);
    let new_top = (current + delta).clamp(0, max_top);
    let actual_delta = new_top - current;

    if actual_delta == 0 {
        return;
    }

    display.view_top.store(new_top, Ordering::Relaxed);
    display.follow.store(new_top == max_top, Ordering::Relaxed);

    let abs_delta = actual_delta.unsigned_abs() as i32;

    surface::draw(|buf| {
        if abs_delta < rows {
            let shift = abs_delta * font::cell_height();
            let width = display.width.load(Ordering::Relaxed);
            let height = display.height.load(Ordering::Relaxed);
            let content_height = rows * font::cell_height();

            if actual_delta < 0 {
                buf.blit(0, 0, 0, shift, width, height - shift);
                let view_top = display.view_top.load(Ordering::Relaxed);
                for row in 0..abs_delta {
                    draw_row_from_scrollback(buf, display, view_top + row, row);
                }
            } else {
                buf.blit(0, shift, 0, 0, width, height - shift);
                let start = rows - abs_delta;
                let view_top = display.view_top.load(Ordering::Relaxed);
                for row in start..rows {
                    draw_row_from_scrollback(buf, display, view_top + row, row);
                }
            }

            // Clear any partial-row pixels below the last full row.
            // height may not be divisible by cell_height, so the blit
            // can leave stale content in the remainder strip.
            if content_height < height {
                gfx::fill_rect(
                    buf,
                    0,
                    content_height,
                    width,
                    height - content_height,
                    load_color(&display.bg),
                );
            }
        } else {
            redraw_view(buf, display);
        }
    });
}

fn half_page_step(display: &DisplayState) -> i32 {
    (display.rows.load(Ordering::Relaxed) / 2).max(1)
}

fn console_page_up(display: &DisplayState) {
    scroll_view(display, -half_page_step(display));
}

fn console_page_down(display: &DisplayState) {
    scroll_view(display, half_page_step(display));
}

fn console_ensure_follow(display: &DisplayState) {
    let max_top =
        (display.total_lines.load(Ordering::Relaxed) - display.rows.load(Ordering::Relaxed)).max(0);
    display.view_top.store(max_top, Ordering::Relaxed);
    display.follow.store(true, Ordering::Relaxed);

    surface::draw(|buf| {
        redraw_view(buf, display);
    });
}

/// Selection range within the input buffer (offsets relative to input, not prompt).
/// When `sel_start == sel_end`, no selection is active.
pub struct InputSelection {
    pub start: usize,
    pub end: usize,
}

impl InputSelection {
    pub const NONE: Self = Self { start: 0, end: 0 };

    pub fn is_active(&self) -> bool {
        self.start != self.end
    }

    pub fn ordered(&self) -> (usize, usize) {
        if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
}

const SELECTION_BG: Color32 = Color32::rgb(0x26, 0x4F, 0x78);

fn console_rewrite_input(
    display: &DisplayState,
    prompt: &[u8],
    prompt_colors: &[u8],
    input: &[u8],
    cursor_pos: usize,
    cursor_visible: bool,
    selection: &InputSelection,
) {
    let cursor_line = display.cursor_line.load(Ordering::Relaxed);
    let slot = display.line_slot(cursor_line);
    let cols = display.cols.load(Ordering::Relaxed) as usize;

    let total_len = prompt.len() + input.len();
    let write_len = total_len.min(cols);

    let mut combined = [0u8; SHELL_SCROLLBACK_COLS];
    let mut colors = [0u8; SHELL_SCROLLBACK_COLS];
    let mut idx = 0;
    for (i, &b) in prompt
        .iter()
        .chain(input.iter())
        .take(write_len)
        .enumerate()
    {
        combined[idx] = b;
        colors[idx] = if i < prompt_colors.len() {
            prompt_colors[i]
        } else {
            COLOR_DEFAULT
        };
        idx += 1;
    }
    scrollback::write_line_colored(slot, &combined[..idx], &colors[..idx]);
    display.cursor_col.store(idx as i32, Ordering::Relaxed);

    if display.follow.load(Ordering::Relaxed) {
        let view_top = display.view_top.load(Ordering::Relaxed);
        let row = cursor_line - view_top;
        if row >= 0 && row < display.rows.load(Ordering::Relaxed) {
            surface::draw(|buf| {
                draw_row_from_scrollback(buf, display, cursor_line, row);

                if selection.is_active() {
                    let (sel_lo, sel_hi) = selection.ordered();
                    let prompt_len = prompt.len();
                    let sel_col_start = (prompt_len + sel_lo) as i32;
                    let sel_col_end = (prompt_len + sel_hi) as i32;

                    for col in sel_col_start..sel_col_end.min(display.cols.load(Ordering::Relaxed))
                    {
                        let ch = if (col as usize) < idx {
                            combined[col as usize]
                        } else {
                            b' '
                        };
                        if ch == 0 {
                            continue;
                        }
                        let fg_idx = scrollback::get_color(slot, col as usize);
                        let fg = PALETTE[fg_idx as usize % PALETTE_SIZE];
                        draw_char_at(buf, col, row, ch, fg, SELECTION_BG);
                    }
                }

                if cursor_visible {
                    let cursor_col = (prompt.len() + cursor_pos) as i32;
                    if cursor_col < display.cols.load(Ordering::Relaxed) {
                        draw_char_at(
                            buf,
                            cursor_col,
                            row,
                            b' ',
                            load_color(&display.bg),
                            load_color(&display.fg),
                        );
                    }
                }
            });
        }
    }
}

// =============================================================================
// Public API functions
// =============================================================================

pub fn shell_console_init() {
    let width = SHELL_WINDOW_WIDTH;
    let height = SHELL_WINDOW_HEIGHT;

    if !surface::init(width, height) {
        panic!("shell: surface init failed");
    }

    DISPLAY.width.store(width, Ordering::Relaxed);
    DISPLAY.height.store(height, Ordering::Relaxed);
    let bytes_pp = surface::bytes_pp();
    DISPLAY.bytes_pp.store(bytes_pp, Ordering::Relaxed);
    DISPLAY
        .pitch
        .store((width as usize) * (bytes_pp as usize), Ordering::Relaxed);

    let cols = width / font::cell_width();
    let rows = height / font::cell_height();
    DISPLAY.cols.store(
        cols.clamp(1, SHELL_SCROLLBACK_COLS as i32),
        Ordering::Relaxed,
    );
    DISPLAY.rows.store(
        rows.clamp(1, SHELL_SCROLLBACK_LINES as i32),
        Ordering::Relaxed,
    );

    if DISPLAY.cols.load(Ordering::Relaxed) <= 0 || DISPLAY.rows.load(Ordering::Relaxed) <= 0 {
        panic!("shell: invalid display dimensions");
    }

    DISPLAY.reset();
    store_color(&DISPLAY.fg, SHELL_FG_COLOR);
    store_color(&DISPLAY.bg, SHELL_BG_COLOR);
}

/// Resize the shell display to new pixel dimensions.
/// Called when a Configure event arrives from the compositor.
pub fn shell_console_resize(new_width: i32, new_height: i32) {
    if new_width <= 0 || new_height <= 0 {
        return;
    }

    // Resize the backing surface
    if !super::surface::resize(new_width as u32, new_height as u32) {
        return;
    }

    DISPLAY.width.store(new_width, Ordering::Relaxed);
    DISPLAY.height.store(new_height, Ordering::Relaxed);
    DISPLAY.pitch.store(
        (new_width as usize) * (DISPLAY.bytes_pp.load(Ordering::Relaxed) as usize),
        Ordering::Relaxed,
    );

    let new_cols = new_width / font::cell_width();
    let new_rows = new_height / font::cell_height();
    DISPLAY.cols.store(
        new_cols.clamp(1, SHELL_SCROLLBACK_COLS as i32),
        Ordering::Relaxed,
    );
    DISPLAY.rows.store(
        new_rows.clamp(1, SHELL_SCROLLBACK_LINES as i32),
        Ordering::Relaxed,
    );

    if DISPLAY.cols.load(Ordering::Relaxed) <= 0 || DISPLAY.rows.load(Ordering::Relaxed) <= 0 {
        return;
    }

    // Clamp cursor to new bounds
    if DISPLAY.cursor_col.load(Ordering::Relaxed) >= DISPLAY.cols.load(Ordering::Relaxed) {
        DISPLAY
            .cursor_col
            .store(DISPLAY.cols.load(Ordering::Relaxed) - 1, Ordering::Relaxed);
    }

    // Adjust view_top so the cursor/input line stays visible after
    // the row count changes -- same as kitty/alacritty on terminal resize.
    let max_top =
        (DISPLAY.total_lines.load(Ordering::Relaxed) - DISPLAY.rows.load(Ordering::Relaxed)).max(0);
    if DISPLAY.follow.load(Ordering::Relaxed) || DISPLAY.view_top.load(Ordering::Relaxed) > max_top
    {
        DISPLAY.view_top.store(max_top, Ordering::Relaxed);
    }

    // Always redraw: surface::resize() allocated a new blank buffer.
    // Skipping the redraw (e.g., when only pixel dims changed but cell
    // count didn't) leaves the buffer black.
    surface::draw(|buf| {
        redraw_view(buf, &DISPLAY);
    });
    shell_console_commit();
}

pub fn shell_console_clear() {
    console_clear(&DISPLAY);
    shell_console_commit();
}

pub fn shell_console_write(buf: &[u8]) {
    console_write(&DISPLAY, buf);
}

pub fn shell_console_write_colored(buf: &[u8], color_idx: u8) {
    let old = current_color_idx();
    set_current_color_idx(color_idx);
    console_write(&DISPLAY, buf);
    set_current_color_idx(old);
}

/// Write output to the current destination (pipe/redirect fd or TTY).
///
/// Returns `true` on success, `false` when the write fails (e.g. broken pipe).
/// Callers in tight loops (like `yes`, `seq`) should check the return value and
/// exit early on `false` to avoid spinning on a dead pipe.
pub fn shell_write(buf: &[u8]) -> bool {
    let redirected_fd = OUTPUT_FD.load(Ordering::Relaxed);
    if redirected_fd >= 0 {
        return fs::write_slice(redirected_fd, buf).is_ok();
    }
    let _ = crate::syscall::tty::write(buf);
    shell_console_write(buf);
    shell_console_commit();
    true
}

/// Write colored text to the current output destination.
///
/// When output is redirected (pipe / file), color is stripped and only the raw
/// text is written.  Otherwise the text goes to the serial TTY (uncolored) and
/// the compositor surface (colored via the palette index matching `fg`).
pub fn shell_write_colored(buf: &[u8], fg: Color32) -> bool {
    let redirected_fd = OUTPUT_FD.load(Ordering::Relaxed);
    if redirected_fd >= 0 {
        return fs::write_slice(redirected_fd, buf).is_ok();
    }
    let _ = crate::syscall::tty::write(buf);
    let idx = palette_index_for(fg);
    shell_console_write_colored(buf, idx);
    shell_console_commit();
    true
}

/// Write text with a palette color index to the current output destination.
///
/// Convenience wrapper that avoids a palette lookup when the caller already
/// has an index.
pub fn shell_write_idx(buf: &[u8], color_idx: u8) -> bool {
    let redirected_fd = OUTPUT_FD.load(Ordering::Relaxed);
    if redirected_fd >= 0 {
        return fs::write_slice(redirected_fd, buf).is_ok();
    }
    let _ = crate::syscall::tty::write(buf);
    shell_console_write_colored(buf, color_idx);
    shell_console_commit();
    true
}

pub fn shell_set_output_fd(fd: i32) {
    OUTPUT_FD.store(fd, Ordering::Relaxed);
}

pub fn shell_clear_output_fd() {
    OUTPUT_FD.store(-1, Ordering::Relaxed);
}

pub fn shell_echo_char(c: u8) {
    let buf = [c];
    let _ = crate::syscall::tty::write(&buf);
    shell_console_write(&buf);
    shell_console_commit();
}

pub fn shell_console_get_cursor() -> (i32, i32) {
    DISPLAY.cursor()
}

pub fn shell_console_page_up() {
    console_page_up(&DISPLAY);
    shell_console_commit();
}

pub fn shell_console_page_down() {
    console_page_down(&DISPLAY);
    shell_console_commit();
}

pub fn shell_console_scroll_lines(lines: i32) {
    if lines != 0 {
        scroll_view(&DISPLAY, lines);
        shell_console_commit();
    }
}

pub fn shell_console_commit() {
    surface::present();
}

pub fn shell_console_follow_bottom() {
    console_ensure_follow(&DISPLAY);
    shell_console_commit();
}

pub fn shell_redraw_input(
    _line_row: i32,
    prompt: &[u8],
    prompt_colors: &[u8],
    input: &[u8],
    cursor_pos: usize,
    cursor_visible: bool,
    selection: &InputSelection,
) {
    console_rewrite_input(
        &DISPLAY,
        prompt,
        prompt_colors,
        input,
        cursor_pos,
        cursor_visible,
        selection,
    );
    shell_console_commit();
}
