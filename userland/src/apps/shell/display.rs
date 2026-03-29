//! Display state, scrollback buffer, and rendering functions.

use core::cell::Cell;

use slopos_abi::draw::Color32;

use crate::gfx::font;
use crate::gfx::{self, DrawBuffer};
use crate::syscall::{DisplayInfo, fs, window};

use super::SyncUnsafeCell;
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
    pub enabled: Cell<bool>,
    pub width: Cell<i32>,
    pub height: Cell<i32>,
    pub pitch: Cell<usize>,
    pub bytes_pp: Cell<u8>,
    pub cols: Cell<i32>,
    pub rows: Cell<i32>,
    pub cursor_col: Cell<i32>,
    pub cursor_line: Cell<i32>,
    pub origin: Cell<i32>,
    pub total_lines: Cell<i32>,
    pub view_top: Cell<i32>,
    pub follow: Cell<bool>,
    pub fg: Cell<Color32>,
    pub bg: Cell<Color32>,
}

impl DisplayState {
    pub const fn new() -> Self {
        Self {
            enabled: Cell::new(false),
            width: Cell::new(0),
            height: Cell::new(0),
            pitch: Cell::new(0),
            bytes_pp: Cell::new(4),
            cols: Cell::new(0),
            rows: Cell::new(0),
            cursor_col: Cell::new(0),
            cursor_line: Cell::new(0),
            origin: Cell::new(0),
            total_lines: Cell::new(1),
            view_top: Cell::new(0),
            follow: Cell::new(true),
            fg: Cell::new(SHELL_FG_COLOR),
            bg: Cell::new(SHELL_BG_COLOR),
        }
    }

    pub fn line_slot(&self, logical: i32) -> usize {
        let max_lines = SHELL_SCROLLBACK_LINES as i32;
        ((self.origin.get() + logical).rem_euclid(max_lines)) as usize
    }

    pub fn cursor(&self) -> (i32, i32) {
        let row = (self.cursor_line.get() - self.view_top.get())
            .clamp(0, self.rows.get().saturating_sub(1));
        (self.cursor_col.get(), row)
    }

    pub fn reset(&self) {
        self.cursor_col.set(0);
        self.cursor_line.set(0);
        self.origin.set(0);
        self.total_lines.set(1);
        self.view_top.set(0);
        self.follow.set(true);
    }
}

// Safety: Userland is single-threaded with no preemption during shell code
unsafe impl Sync for DisplayState {}

pub static DISPLAY: DisplayState = DisplayState::new();
static OUTPUT_FD: SyncUnsafeCell<i32> = SyncUnsafeCell::new(-1);
static CURRENT_COLOR_IDX: SyncUnsafeCell<u8> = SyncUnsafeCell::new(0);

#[inline]
fn current_color_idx() -> u8 {
    unsafe { *CURRENT_COLOR_IDX.get() }
}

#[inline]
fn set_current_color_idx(idx: u8) {
    unsafe { *CURRENT_COLOR_IDX.get() = idx }
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

    static DATA: SyncUnsafeCell<[u8; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS]> =
        SyncUnsafeCell::new([0; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS]);

    static COLORS: SyncUnsafeCell<[u8; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS]> =
        SyncUnsafeCell::new([0; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS]);

    static LENS: SyncUnsafeCell<[u16; SHELL_SCROLLBACK_LINES]> =
        SyncUnsafeCell::new([0; SHELL_SCROLLBACK_LINES]);

    #[inline]
    pub fn with_line<R, F: FnOnce(&[u8]) -> R>(slot: usize, f: F) -> R {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe {
            let data = &*DATA.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            f(&data[start..start + SHELL_SCROLLBACK_COLS])
        }
    }

    #[inline]
    pub fn get_line_len(slot: usize) -> u16 {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe { (*LENS.get())[slot] }
    }

    #[inline]
    pub fn set_line_len(slot: usize, len: u16) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe { (*LENS.get())[slot] = len }
    }

    #[inline]
    pub fn set_char(slot: usize, col: usize, ch: u8) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        unsafe {
            let data = &mut *DATA.get();
            data[slot * SHELL_SCROLLBACK_COLS + col] = ch;
        }
    }

    #[inline]
    pub fn get_char(slot: usize, col: usize) -> u8 {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        unsafe {
            let data = &*DATA.get();
            data[slot * SHELL_SCROLLBACK_COLS + col]
        }
    }

    #[inline]
    pub fn set_color(slot: usize, col: usize, color_idx: u8) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        unsafe {
            let colors = &mut *COLORS.get();
            colors[slot * SHELL_SCROLLBACK_COLS + col] = color_idx;
        }
    }

    #[inline]
    pub fn get_color(slot: usize, col: usize) -> u8 {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        unsafe {
            let colors = &*COLORS.get();
            colors[slot * SHELL_SCROLLBACK_COLS + col]
        }
    }

    pub fn clear_line(slot: usize) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe {
            let data = &mut *DATA.get();
            let colors = &mut *COLORS.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            for i in start..start + SHELL_SCROLLBACK_COLS {
                data[i] = 0;
                colors[i] = 0;
            }
            (*LENS.get())[slot] = 0;
        }
    }

    pub fn clear_all() {
        unsafe {
            let data = &mut *DATA.get();
            for byte in data.iter_mut() {
                *byte = 0;
            }
            let colors = &mut *COLORS.get();
            for c in colors.iter_mut() {
                *c = 0;
            }
            let lens = &mut *LENS.get();
            for len in lens.iter_mut() {
                *len = 0;
            }
        }
    }

    pub fn write_line(slot: usize, content: &[u8]) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let len = content.len().min(SHELL_SCROLLBACK_COLS);
        unsafe {
            let data = &mut *DATA.get();
            let colors = &mut *COLORS.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            for i in start..start + SHELL_SCROLLBACK_COLS {
                data[i] = 0;
                colors[i] = 0;
            }
            for (i, &b) in content.iter().take(len).enumerate() {
                data[start + i] = b;
            }
            (*LENS.get())[slot] = len as u16;
        }
    }

    pub fn write_line_colored(slot: usize, content: &[u8], color_indices: &[u8]) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let len = content.len().min(SHELL_SCROLLBACK_COLS);
        unsafe {
            let data = &mut *DATA.get();
            let colors = &mut *COLORS.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            for i in start..start + SHELL_SCROLLBACK_COLS {
                data[i] = 0;
                colors[i] = 0;
            }
            for (i, &b) in content.iter().take(len).enumerate() {
                data[start + i] = b;
                if i < color_indices.len() {
                    colors[start + i] = color_indices[i];
                }
            }
            (*LENS.get())[slot] = len as u16;
        }
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
    let bg = display.bg.get();
    let width = display.width.get();
    let cols = display.cols.get();
    let total_lines = display.total_lines.get();

    clear_row(buf, row, width, bg);

    if logical < 0 || logical >= total_lines {
        return;
    }

    let slot = display.line_slot(logical);
    let len = scrollback::get_line_len(slot) as usize;
    let draw_len = len.min(cols as usize);

    if draw_len == 0 {
        return;
    }

    scrollback::with_line(slot, |line| {
        for (col, &ch) in line.iter().take(draw_len).enumerate() {
            if ch != 0 {
                let color_idx = scrollback::get_color(slot, col);
                let fg = PALETTE[color_idx as usize % PALETTE_SIZE];
                draw_char_at(buf, col as i32, row, ch, fg, bg);
            }
        }
    });
}

fn redraw_view(buf: &mut DrawBuffer, display: &DisplayState) {
    let bg = display.bg.get();
    let width = display.width.get();
    let height = display.height.get();
    let rows = display.rows.get();
    let view_top = display.view_top.get();

    // Clear entire view
    gfx::fill_rect(buf, 0, 0, width, height, bg);

    // Draw each row
    for row in 0..rows {
        draw_row_from_scrollback(buf, display, view_top + row, row);
    }
}

fn scroll_up_fast(buf: &mut DrawBuffer, display: &DisplayState) -> bool {
    let width = display.width.get();
    let height = display.height.get();
    let bg = display.bg.get();

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
    display.cursor_col.set(0);
    let cursor_line = display.cursor_line.get() + 1;
    display.cursor_line.set(cursor_line);

    let total_lines = display.total_lines.get();
    if cursor_line >= total_lines {
        if total_lines < SHELL_SCROLLBACK_LINES as i32 {
            display.total_lines.set(total_lines + 1);
        } else {
            let origin = (display.origin.get() + 1) % SHELL_SCROLLBACK_LINES as i32;
            display.origin.set(origin);
            display.cursor_line.set(total_lines - 1);
            let view_top = display.view_top.get();
            if view_top > 0 {
                display.view_top.set(view_top - 1);
            }
        }
        let slot = display.line_slot(display.cursor_line.get());
        scrollback::clear_line(slot);
    }
}

fn update_char_state(display: &DisplayState, c: u8) {
    let cursor_col = display.cursor_col.get();
    let cursor_line = display.cursor_line.get();
    let cols = display.cols.get();
    let slot = display.line_slot(cursor_line);

    if (cursor_col as usize) < SHELL_SCROLLBACK_COLS {
        scrollback::set_char(slot, cursor_col as usize, c);
        scrollback::set_color(slot, cursor_col as usize, current_color_idx());
        let len = scrollback::get_line_len(slot) as i32;
        if cursor_col + 1 > len {
            scrollback::set_line_len(slot, (cursor_col + 1) as u16);
        }
    }

    display.cursor_col.set(cursor_col + 1);
    if display.cursor_col.get() >= cols {
        update_new_line(display);
    }
}

fn update_backspace_state(display: &DisplayState) {
    let mut cursor_col = display.cursor_col.get();
    let mut cursor_line = display.cursor_line.get();

    if cursor_col > 0 {
        cursor_col -= 1;
    } else if cursor_line > 0 {
        cursor_line -= 1;
        let slot = display.line_slot(cursor_line);
        let len = scrollback::get_line_len(slot) as i32;
        cursor_col = if len > 0 {
            (len - 1).clamp(0, display.cols.get().saturating_sub(1))
        } else {
            0
        };
    } else {
        return;
    }

    display.cursor_col.set(cursor_col);
    display.cursor_line.set(cursor_line);

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
    if !display.enabled.get() {
        return;
    }

    let follow = display.follow.get();
    let mut needs_scroll = false;
    let old_view_top = display.view_top.get();
    let start_line = display.cursor_line.get(); // Track first line modified

    // Phase 1: Update state
    for &b in text {
        match b {
            b'\n' => {
                let old_total = display.total_lines.get();
                update_new_line(display);

                // Check if we need to scroll view
                if follow {
                    let rows = display.rows.get();
                    let total = display.total_lines.get();
                    let max_top = (total - rows).max(0);
                    if display.view_top.get() != max_top {
                        display.view_top.set(max_top);
                        if total > old_total || display.view_top.get() != old_view_top {
                            needs_scroll = true;
                        }
                    }
                }
            }
            b'\r' => display.cursor_col.set(0),
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
            let view_top = display.view_top.get();
            let rows = display.rows.get();
            let cursor_line = display.cursor_line.get();

            if needs_scroll {
                let view_diff = display.view_top.get() - old_view_top;
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
    if !display.enabled.get() {
        return;
    }

    display.reset();
    scrollback::clear_all();

    surface::draw(|buf| {
        let bg = display.bg.get();
        let width = display.width.get();
        let height = display.height.get();
        gfx::fill_rect(buf, 0, 0, width, height, bg);
    });
}

fn scroll_view(display: &DisplayState, delta: i32) {
    let rows = display.rows.get();
    let total_lines = display.total_lines.get();

    if total_lines <= rows {
        return;
    }

    let max_top = (total_lines - rows).max(0);
    let current = display.view_top.get();
    let new_top = (current + delta).clamp(0, max_top);
    let actual_delta = new_top - current;

    if actual_delta == 0 {
        return;
    }

    display.view_top.set(new_top);
    display.follow.set(new_top == max_top);

    let abs_delta = actual_delta.unsigned_abs() as i32;

    surface::draw(|buf| {
        if abs_delta < rows {
            let shift = abs_delta * font::cell_height();
            let width = display.width.get();
            let height = display.height.get();

            if actual_delta < 0 {
                buf.blit(0, 0, 0, shift, width, height - shift);
                let view_top = display.view_top.get();
                for row in 0..abs_delta {
                    draw_row_from_scrollback(buf, display, view_top + row, row);
                }
            } else {
                buf.blit(0, shift, 0, 0, width, height - shift);
                let start = rows - abs_delta;
                let view_top = display.view_top.get();
                for row in start..rows {
                    draw_row_from_scrollback(buf, display, view_top + row, row);
                }
            }
        } else {
            redraw_view(buf, display);
        }
    });
}

fn half_page_step(display: &DisplayState) -> i32 {
    (display.rows.get() / 2).max(1)
}

fn console_page_up(display: &DisplayState) {
    scroll_view(display, -half_page_step(display));
}

fn console_page_down(display: &DisplayState) {
    scroll_view(display, half_page_step(display));
}

fn console_ensure_follow(display: &DisplayState) {
    let max_top = (display.total_lines.get() - display.rows.get()).max(0);
    display.view_top.set(max_top);
    display.follow.set(true);

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
    if !display.enabled.get() {
        return;
    }

    let cursor_line = display.cursor_line.get();
    let slot = display.line_slot(cursor_line);
    let cols = display.cols.get() as usize;

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
    display.cursor_col.set(idx as i32);

    if display.follow.get() {
        let view_top = display.view_top.get();
        let row = cursor_line - view_top;
        if row >= 0 && row < display.rows.get() {
            surface::draw(|buf| {
                draw_row_from_scrollback(buf, display, cursor_line, row);

                if selection.is_active() {
                    let (sel_lo, sel_hi) = selection.ordered();
                    let prompt_len = prompt.len();
                    let sel_col_start = (prompt_len + sel_lo) as i32;
                    let sel_col_end = (prompt_len + sel_hi) as i32;

                    for col in sel_col_start..sel_col_end.min(display.cols.get()) {
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
                    if cursor_col < display.cols.get() {
                        draw_char_at(
                            buf,
                            cursor_col,
                            row,
                            b' ',
                            display.bg.get(),
                            display.fg.get(),
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
        DISPLAY.enabled.set(false);
        return;
    }

    let mut info = DisplayInfo::default();
    let _ = window::fb_info(&mut info);

    DISPLAY.width.set(width);
    DISPLAY.height.set(height);
    let bytes_pp = info.bytes_per_pixel();
    DISPLAY.bytes_pp.set(bytes_pp);
    DISPLAY.pitch.set((width as usize) * (bytes_pp as usize));

    let cols = width / font::cell_width();
    let rows = height / font::cell_height();
    DISPLAY
        .cols
        .set(cols.clamp(1, SHELL_SCROLLBACK_COLS as i32));
    DISPLAY
        .rows
        .set(rows.clamp(1, SHELL_SCROLLBACK_LINES as i32));

    if DISPLAY.cols.get() <= 0 || DISPLAY.rows.get() <= 0 {
        DISPLAY.enabled.set(false);
        return;
    }

    DISPLAY.enabled.set(true);
    DISPLAY.reset();
    DISPLAY.fg.set(SHELL_FG_COLOR);
    DISPLAY.bg.set(SHELL_BG_COLOR);
}

/// Resize the shell display to new pixel dimensions.
/// Called when a Configure event arrives from the compositor.
pub fn shell_console_resize(new_width: i32, new_height: i32) {
    if !DISPLAY.enabled.get() || new_width <= 0 || new_height <= 0 {
        return;
    }

    // Resize the backing surface
    if !super::surface::resize(new_width as u32, new_height as u32) {
        return;
    }

    DISPLAY.width.set(new_width);
    DISPLAY.height.set(new_height);
    DISPLAY
        .pitch
        .set((new_width as usize) * (DISPLAY.bytes_pp.get() as usize));

    let new_cols = new_width / font::cell_width();
    let new_rows = new_height / font::cell_height();
    DISPLAY
        .cols
        .set(new_cols.clamp(1, SHELL_SCROLLBACK_COLS as i32));
    DISPLAY
        .rows
        .set(new_rows.clamp(1, SHELL_SCROLLBACK_LINES as i32));

    if DISPLAY.cols.get() <= 0 || DISPLAY.rows.get() <= 0 {
        DISPLAY.enabled.set(false);
        return;
    }

    // Clamp cursor to new bounds
    if DISPLAY.cursor_col.get() >= DISPLAY.cols.get() {
        DISPLAY.cursor_col.set(DISPLAY.cols.get() - 1);
    }

    // Adjust view_top so the cursor/input line stays visible after
    // the row count changes — same as kitty/alacritty on terminal resize.
    let max_top = (DISPLAY.total_lines.get() - DISPLAY.rows.get()).max(0);
    if DISPLAY.follow.get() || DISPLAY.view_top.get() > max_top {
        DISPLAY.view_top.set(max_top);
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
    if DISPLAY.enabled.get() {
        console_clear(&DISPLAY);
        shell_console_commit();
    }
}

pub fn shell_console_write(buf: &[u8]) {
    if DISPLAY.enabled.get() {
        console_write(&DISPLAY, buf);
    }
}

pub fn shell_console_write_colored(buf: &[u8], color_idx: u8) {
    if DISPLAY.enabled.get() {
        let old = current_color_idx();
        set_current_color_idx(color_idx);
        console_write(&DISPLAY, buf);
        set_current_color_idx(old);
    }
}

/// Write output to the current destination (pipe/redirect fd or TTY).
///
/// Returns `true` on success, `false` when the write fails (e.g. broken pipe).
/// Callers in tight loops (like `yes`, `seq`) should check the return value and
/// exit early on `false` to avoid spinning on a dead pipe.
pub fn shell_write(buf: &[u8]) -> bool {
    let redirected_fd = unsafe { *OUTPUT_FD.get() };
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
    let redirected_fd = unsafe { *OUTPUT_FD.get() };
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
    let redirected_fd = unsafe { *OUTPUT_FD.get() };
    if redirected_fd >= 0 {
        return fs::write_slice(redirected_fd, buf).is_ok();
    }
    let _ = crate::syscall::tty::write(buf);
    shell_console_write_colored(buf, color_idx);
    shell_console_commit();
    true
}

pub fn shell_set_output_fd(fd: i32) {
    unsafe {
        *OUTPUT_FD.get() = fd;
    }
}

pub fn shell_clear_output_fd() {
    unsafe {
        *OUTPUT_FD.get() = -1;
    }
}

pub fn shell_echo_char(c: u8) {
    let buf = [c];
    let _ = crate::syscall::tty::write(&buf);
    shell_console_write(&buf);
    shell_console_commit();
}

pub fn shell_console_get_cursor() -> (i32, i32) {
    if DISPLAY.enabled.get() {
        DISPLAY.cursor()
    } else {
        (0, 0)
    }
}

pub fn shell_console_page_up() {
    if DISPLAY.enabled.get() {
        console_page_up(&DISPLAY);
        shell_console_commit();
    }
}

pub fn shell_console_page_down() {
    if DISPLAY.enabled.get() {
        console_page_down(&DISPLAY);
        shell_console_commit();
    }
}

pub fn shell_console_commit() {
    if DISPLAY.enabled.get() {
        surface::present_full();
    }
}

pub fn shell_console_follow_bottom() {
    if DISPLAY.enabled.get() {
        console_ensure_follow(&DISPLAY);
        shell_console_commit();
    }
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
    if DISPLAY.enabled.get() {
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
}
