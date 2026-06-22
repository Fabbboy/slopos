//! Terminal VT interpreter — a userland port of the kernel vconsole's pure
//! grid logic, driven by the `slopos-vt` parser.
//!
//! Holds a `Cell` grid, ANSI color tables (standard 8 + bright 8 + 256-color),
//! cursor attributes, alt-screen, scroll region, and a scrollback ring. All
//! kernel-isms (framebuffer blits, atlas globals, dirty bitmasks, SpinLock
//! statics) are gone: this module owns terminal *state* only; `render.rs`
//! reads the grid and paints the surface.

use alloc::vec;
use alloc::vec::Vec;

use slopos_abi::unicode::is_double_width;

use slopos_vt::{Direction, EraseMode, SgrAttr, VtAction, VtParser};

pub const MAX_COLS: usize = 240;
pub const MAX_ROWS: usize = 100;

/// Default foreground (the shell's classic light grey).
pub const FG_COLOR: u32 = 0x00E6_E6E6;
/// Default background (the shell's classic dark grey, not ANSI black —
/// explicit SGR 40 still renders pure black).
pub const BG_COLOR: u32 = 0x001E_1E1E;

const SCROLLBACK_LINES: usize = 1000;

/// Continuation marker for the right half of a double-width character.
const CONTINUATION_CODEPOINT: u32 = 0xFFFF_FFFF;

// ---------------------------------------------------------------------------
// ANSI color tables (standard 8 + bright 8)
// ---------------------------------------------------------------------------

const ANSI_COLORS: [u32; 8] = [
    0x0000_0000, // Black
    0x00AA_0000, // Red
    0x0000_AA00, // Green
    0x00AA_5500, // Yellow / Brown
    0x0000_00AA, // Blue
    0x00AA_00AA, // Magenta
    0x0000_AAAA, // Cyan
    0x00AA_AAAA, // White
];

const ANSI_BRIGHT_COLORS: [u32; 8] = [
    0x0055_5555, // Bright Black (Gray)
    0x00FF_5555, // Bright Red
    0x0055_FF55, // Bright Green
    0x00FF_FF55, // Bright Yellow
    0x0055_55FF, // Bright Blue
    0x00FF_55FF, // Bright Magenta
    0x0055_FFFF, // Bright Cyan
    0x00FF_FFFF, // Bright White
];

/// Map an xterm 256-color index to a packed `0x00RRGGBB` value.
fn color256_to_rgb(idx: u8) -> u32 {
    match idx {
        0..=7 => ANSI_COLORS[idx as usize],
        8..=15 => ANSI_BRIGHT_COLORS[(idx - 8) as usize],
        16..=231 => {
            // 6x6x6 color cube: index = 16 + 36*r + 6*g + b (each 0..5)
            let n = (idx - 16) as u32;
            let b = (n % 6) * 51;
            let g = ((n / 6) % 6) * 51;
            let r = (n / 36) * 51;
            (r << 16) | (g << 8) | b
        }
        232..=255 => {
            // Grayscale ramp: 24 shades from 8 to 238.
            let v = (8 + 10 * (idx - 232) as u32) & 0xFF;
            (v << 16) | (v << 8) | v
        }
    }
}

// ---------------------------------------------------------------------------
// Per-cell and cursor attribute types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CellAttributes {
    pub fg: u32,
    pub bg: u32,
}

impl CellAttributes {
    pub const fn default_colors() -> Self {
        Self {
            fg: FG_COLOR,
            bg: BG_COLOR,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Cell {
    pub codepoint: u32,
    pub attrs: CellAttributes,
}

impl Cell {
    pub const fn blank() -> Self {
        Self {
            codepoint: b' ' as u32,
            attrs: CellAttributes::default_colors(),
        }
    }

    /// Codepoint with the double-width continuation sentinel folded to a space
    /// so a renderer can blit it directly.
    #[inline]
    pub fn glyph(&self) -> u32 {
        if self.codepoint == CONTINUATION_CODEPOINT {
            b' ' as u32
        } else {
            self.codepoint
        }
    }
}

/// Reserve `buf`'s capacity straight to `max` the first time it would grow past
/// what it already holds, so a run of incremental `resize`s (a terminal resize
/// drag) reallocates at most once over the buffer's life instead of every step.
/// The backing is lazy — reserving address space faults in no pages — so only
/// the live (`len`-sized) region costs physical memory.
fn reserve_to_max<T>(buf: &mut Vec<T>, max: usize) {
    if buf.capacity() < max {
        buf.reserve_exact(max.saturating_sub(buf.len()));
    }
}

/// Flat row-major `Cell` buffer.
pub struct CellGrid {
    cells: Vec<Cell>,
    cols: usize,
}

impl CellGrid {
    pub fn empty() -> Self {
        Self {
            cells: Vec::new(),
            cols: 0,
        }
    }

    /// Re-shape to `rows`×`cols`, blanking the whole grid. The capacity grows
    /// at most once (to the maximum grid), so reusing one `CellGrid` across a
    /// resize drag never reallocates after that — `len` still tracks the live
    /// area, so every other method stays bound to the logical grid.
    pub fn allocate(&mut self, rows: usize, cols: usize) {
        let total = rows.saturating_mul(cols);
        if total == 0 {
            self.cells.clear();
            self.cols = 0;
            return;
        }
        reserve_to_max(&mut self.cells, MAX_ROWS * MAX_COLS);
        self.cells.clear();
        self.cells.resize(total, Cell::blank());
        self.cols = cols;
    }

    #[inline]
    pub fn get(&self, row: usize, col: usize) -> Cell {
        let idx = row * self.cols + col;
        self.cells.get(idx).copied().unwrap_or_else(Cell::blank)
    }

    #[inline]
    pub fn set(&mut self, row: usize, col: usize, cell: Cell) {
        let idx = row * self.cols + col;
        if idx < self.cells.len() {
            self.cells[idx] = cell;
        }
    }

    fn row_copy(&mut self, dst_row: usize, src_row: usize, n_cols: usize) {
        let n = n_cols.min(self.cols);
        let src = src_row * self.cols;
        let dst = dst_row * self.cols;
        if src + n <= self.cells.len() && dst + n <= self.cells.len() {
            self.cells.copy_within(src..src + n, dst);
        }
    }

    fn copy_from(&mut self, other: &CellGrid, rows: usize, cols: usize) {
        for r in 0..rows {
            for c in 0..cols {
                self.set(r, c, other.get(r, c));
            }
        }
    }

    fn clear_all(&mut self) {
        for cell in self.cells.iter_mut() {
            *cell = Cell::blank();
        }
    }
}

#[derive(Clone, Copy)]
pub struct CursorAttributes {
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl CursorAttributes {
    pub const fn default_attrs() -> Self {
        Self {
            fg: FG_COLOR,
            bg: BG_COLOR,
            bold: false,
            underline: false,
            inverse: false,
        }
    }

    fn effective_fg(&self) -> u32 {
        if self.inverse {
            self.bg
        } else if self.bold {
            self.brighten(self.fg)
        } else {
            self.fg
        }
    }

    fn effective_bg(&self) -> u32 {
        if self.inverse {
            if self.bold {
                self.brighten(self.fg)
            } else {
                self.fg
            }
        } else {
            self.bg
        }
    }

    fn brighten(&self, color: u32) -> u32 {
        for i in 0..8 {
            if ANSI_COLORS[i] == color {
                return ANSI_BRIGHT_COLORS[i];
            }
        }
        color
    }
}

// ---------------------------------------------------------------------------
// Scrollback ring
// ---------------------------------------------------------------------------

struct ScrollbackBuf {
    buf: Vec<Cell>,
    cols: usize,
    head: usize,
    count: usize,
    view_offset: usize,
}

impl ScrollbackBuf {
    fn new(cols: usize) -> Self {
        let total = SCROLLBACK_LINES.saturating_mul(cols);
        let buf = if total > 0 {
            vec![Cell::blank(); total]
        } else {
            Vec::new()
        };
        Self {
            buf,
            cols: cols.min(MAX_COLS),
            head: 0,
            count: 0,
            view_offset: 0,
        }
    }

    /// Re-stride the ring to `cols` and drop history. A width change cannot
    /// reflow fixed-width rows, so history is discarded; the retained cells
    /// stay unread because resetting `count` gates every reader and `push_row`
    /// fully overwrites a row before `get_row` can serve it. The backing only
    /// ever grows — and jumps straight to its `MAX_COLS` ceiling the first time
    /// it must — so a resize drag never reallocates this multi-megabyte ring.
    fn reset_for_width(&mut self, cols: usize) {
        let cols = cols.min(MAX_COLS);
        let needed = SCROLLBACK_LINES.saturating_mul(cols);
        if self.buf.len() < needed {
            reserve_to_max(&mut self.buf, SCROLLBACK_LINES * MAX_COLS);
            self.buf.resize(needed, Cell::blank());
        }
        self.cols = cols;
        self.head = 0;
        self.count = 0;
        self.view_offset = 0;
    }

    fn push_row(&mut self, cells: &CellGrid, row: usize, cols: usize) {
        if self.buf.is_empty() || cols == 0 || self.cols == 0 {
            return;
        }
        let start = self.head * self.cols;
        let end = start + self.cols;
        if end > self.buf.len() {
            return;
        }
        let n = cols.min(self.cols);
        for c in 0..n {
            self.buf[start + c] = cells.get(row, c);
        }
        for slot in self.buf[start + n..end].iter_mut() {
            *slot = Cell::blank();
        }
        self.head = (self.head + 1) % SCROLLBACK_LINES;
        if self.count < SCROLLBACK_LINES {
            self.count += 1;
        }
    }

    fn get_row(&self, offset_from_bottom: usize) -> Option<&[Cell]> {
        if offset_from_bottom == 0 || offset_from_bottom > self.count || self.cols == 0 {
            return None;
        }
        let idx = (self.head + SCROLLBACK_LINES - offset_from_bottom) % SCROLLBACK_LINES;
        let start = idx * self.cols;
        let end = start + self.cols;
        if end > self.buf.len() {
            return None;
        }
        Some(&self.buf[start..end])
    }

    fn scroll_up(&mut self, lines: usize) {
        self.view_offset = (self.view_offset + lines).min(self.count);
    }

    fn scroll_down(&mut self, lines: usize) {
        self.view_offset = self.view_offset.saturating_sub(lines);
    }

    fn reset_view(&mut self) {
        self.view_offset = 0;
    }
}

// ---------------------------------------------------------------------------
// Terminal grid state
// ---------------------------------------------------------------------------

pub struct TerminalGrid {
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub rows: u16,
    pub cols: u16,
    cells: CellGrid,
    /// Pooled grid swapped in on resize so the live screen and alt buffers are
    /// re-shaped by reusing one extra allocation instead of allocating a fresh
    /// grid per resize step (a drag fires one resize per cell-width crossed).
    resize_scratch: CellGrid,
    parser: VtParser,
    cursor_attrs: CursorAttributes,
    saved_cursor_row: u16,
    saved_cursor_col: u16,
    saved_cursor_attrs: CursorAttributes,
    pub cursor_visible: bool,
    alt_cells: CellGrid,
    alt_screen_cursor_row: u16,
    alt_screen_cursor_col: u16,
    in_alt_screen: bool,
    /// DECSET/DECRST 2004: the slave-side app asked for paste bracketing.
    bracketed_paste: bool,
    scroll_top: u16,
    scroll_bottom: u16,
    scrollback: ScrollbackBuf,
    /// Count of lines ever evicted into scrollback. This is the absolute line
    /// number of live screen row 0, giving every line (history + live) a
    /// stable identity that survives scrolling — selections anchor to it so a
    /// copy grabs the originally-selected text even after output scrolls.
    total_scrolled: u64,
    /// VT100 deferred autowrap: a char printed in the last column leaves the
    /// cursor there with the wrap pending; the wrap commits only when the
    /// next printable char arrives. An exactly-full line followed by `\r\n`
    /// therefore advances one row, not two.
    wrap_pending: bool,
    /// Set whenever the visible grid changes; the event loop renders on it.
    dirty: bool,
}

impl TerminalGrid {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.clamp(1, MAX_ROWS as u16);
        let cols = cols.clamp(1, MAX_COLS as u16);
        let mut cells = CellGrid::empty();
        cells.allocate(rows as usize, cols as usize);
        let mut alt_cells = CellGrid::empty();
        alt_cells.allocate(rows as usize, cols as usize);
        Self {
            cursor_row: 0,
            cursor_col: 0,
            rows,
            cols,
            cells,
            resize_scratch: CellGrid::empty(),
            parser: VtParser::new(),
            cursor_attrs: CursorAttributes::default_attrs(),
            saved_cursor_row: 0,
            saved_cursor_col: 0,
            saved_cursor_attrs: CursorAttributes::default_attrs(),
            cursor_visible: true,
            alt_cells,
            alt_screen_cursor_row: 0,
            alt_screen_cursor_col: 0,
            in_alt_screen: false,
            bracketed_paste: false,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            scrollback: ScrollbackBuf::new(cols as usize),
            total_scrolled: 0,
            wrap_pending: false,
            dirty: true,
        }
    }

    /// True when the visible content changed since the last call; clears the
    /// flag so the next call reports only fresh changes.
    #[inline]
    pub fn take_dirty(&mut self) -> bool {
        let d = self.dirty;
        self.dirty = false;
        d
    }

    /// Snapshot of a visible cell, accounting for any active scrollback view.
    #[inline]
    pub fn visible_cell(&self, row: usize, col: usize) -> Cell {
        let sb_lines = self.scrollback.view_offset.min(self.rows as usize);
        if row < sb_lines {
            // Rows above the live region show scrollback history.
            let sb_offset = self.scrollback.view_offset - row;
            if let Some(sb_row) = self.scrollback.get_row(sb_offset) {
                return sb_row.get(col).copied().unwrap_or_else(Cell::blank);
            }
            return Cell::blank();
        }
        let live_row = row - sb_lines;
        self.cells.get(live_row, col)
    }

    /// Absolute line number shown at visible screen `row`, given the current
    /// scrollback view. Exact inverse of [`visible_cell`](Self::visible_cell)'s
    /// `sb_lines` / `view_offset` split — the single authority both render
    /// (highlight) and copy (collect) convert through, so they never disagree.
    #[inline]
    pub fn screen_to_abs(&self, row: usize) -> u64 {
        let sb_lines = self.scrollback.view_offset.min(self.rows as usize);
        if row < sb_lines {
            // Scrollback line: `view_offset - row` is its offset-from-bottom.
            let sb_offset = (self.scrollback.view_offset - row) as u64;
            self.total_scrolled.wrapping_sub(sb_offset)
        } else {
            let live_row = (row - sb_lines) as u64;
            self.total_scrolled.wrapping_add(live_row)
        }
    }

    /// Visible screen row currently showing absolute line `abs`, or `None` when
    /// `abs` is scrolled off-screen or evicted from the scrollback ring.
    pub fn abs_to_screen(&self, abs: u64) -> Option<usize> {
        let rows = self.rows as usize;
        let view_offset = self.scrollback.view_offset;
        let sb_lines = view_offset.min(rows);
        if abs >= self.total_scrolled {
            // Live line; pushed down by `sb_lines` when scrolled into history.
            let live_row = (abs - self.total_scrolled) as usize;
            let screen = live_row + sb_lines;
            (screen < rows).then_some(screen)
        } else {
            // Scrollback line: `k` = offset-from-bottom (>= 1).
            let k = (self.total_scrolled - abs) as usize;
            if k > self.scrollback.count || k > view_offset {
                return None;
            }
            let screen = view_offset - k;
            (screen < sb_lines).then_some(screen)
        }
    }

    /// Cell at an absolute `(line, col)`, resolving to the live buffer or the
    /// scrollback ring independent of the current view. Blank for an evicted
    /// or out-of-range line — copy degrades gracefully, never panics.
    pub fn abs_cell(&self, abs_line: u64, col: usize) -> Cell {
        if abs_line >= self.total_scrolled {
            let live_row = (abs_line - self.total_scrolled) as usize;
            if live_row < self.rows as usize {
                self.cells.get(live_row, col)
            } else {
                Cell::blank()
            }
        } else {
            let k = (self.total_scrolled - abs_line) as usize;
            match self.scrollback.get_row(k) {
                Some(r) => r.get(col).copied().unwrap_or_else(Cell::blank),
                None => Cell::blank(),
            }
        }
    }

    /// True when the user has scrolled into history (cursor must not render).
    #[inline]
    pub fn viewing_history(&self) -> bool {
        self.scrollback.view_offset > 0
    }

    /// True when the slave-side app enabled bracketed paste (DECSET 2004).
    #[inline]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    // -----------------------------------------------------------------------
    // Local scrollback control (Shift+PgUp / Shift+PgDn).
    // -----------------------------------------------------------------------

    pub fn scroll_view_up(&mut self, lines: usize) {
        self.scrollback.scroll_up(lines);
        self.dirty = true;
    }

    pub fn scroll_view_down(&mut self, lines: usize) {
        self.scrollback.scroll_down(lines);
        self.dirty = true;
    }

    // -----------------------------------------------------------------------
    // Resize
    // -----------------------------------------------------------------------

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.clamp(1, MAX_ROWS as u16);
        let cols = cols.clamp(1, MAX_COLS as u16);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.wrap_pending = false;

        // Height shrink anchors the cursor, not the top: when the new bottom
        // would cut the primary cursor off, the displaced top rows scroll
        // into history and the screen shifts up so the cursor's line stays
        // on screen. A top-aligned clip would teleport the physical cursor
        // away from the line an application (e.g. a shell line editor) is
        // tracking, corrupting every relative-cursor redraw that follows.
        // The alt screen has no history; its cursor is clamped below.
        let primary_cursor_row = if self.in_alt_screen {
            self.alt_screen_cursor_row
        } else {
            self.cursor_row
        };
        let shift = if rows < self.rows && primary_cursor_row >= rows {
            (primary_cursor_row - rows + 1) as usize
        } else {
            0
        };
        if shift > 0 {
            let old_rows = self.rows as usize;
            let old_cols = self.cols as usize;
            // While in the alt screen, `cells` holds the alt content and the
            // saved primary screen lives in `alt_cells` (copy-on-enter) —
            // the shift must follow the primary buffer either way.
            let primary = if self.in_alt_screen {
                &self.alt_cells
            } else {
                &self.cells
            };
            for r in 0..shift {
                // Pushed at the old width; a simultaneous width change
                // resets the ring below, matching the existing
                // width-change-drops-scrollback behavior.
                self.scrollback.push_row(primary, r, old_cols);
                self.total_scrolled = self.total_scrolled.wrapping_add(1);
            }
            self.scrollback.reset_view();
            let primary = if self.in_alt_screen {
                &mut self.alt_cells
            } else {
                &mut self.cells
            };
            for dst in 0..(old_rows - shift) {
                primary.row_copy(dst, dst + shift, old_cols);
            }
            if self.in_alt_screen {
                self.alt_screen_cursor_row -= shift as u16;
            } else {
                self.cursor_row -= shift as u16;
            }
        }

        let copy_rows = (self.rows.min(rows)) as usize;
        let copy_cols = (self.cols.min(cols)) as usize;

        // Re-shape both grids by reusing the pooled scratch: `allocate` blanks
        // it to the new size, `copy_from` carries the overlap forward, and the
        // swap installs it while parking the old backing in the pool for the
        // next resize. No grid is allocated per resize step.
        self.resize_scratch.allocate(rows as usize, cols as usize);
        self.resize_scratch
            .copy_from(&self.cells, copy_rows, copy_cols);
        core::mem::swap(&mut self.cells, &mut self.resize_scratch);

        self.resize_scratch.allocate(rows as usize, cols as usize);
        self.resize_scratch
            .copy_from(&self.alt_cells, copy_rows, copy_cols);
        core::mem::swap(&mut self.alt_cells, &mut self.resize_scratch);

        if self.scrollback.cols != cols as usize {
            self.scrollback.reset_for_width(cols as usize);
        }
        self.rows = rows;
        self.cols = cols;
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        if self.cursor_col >= self.cols {
            self.cursor_col = self.cols.saturating_sub(1);
        }
        if self.cursor_row >= self.rows {
            self.cursor_row = self.rows.saturating_sub(1);
        }
        // The parked cursor of the inactive screen obeys the same bounds.
        if self.alt_screen_cursor_col >= self.cols {
            self.alt_screen_cursor_col = self.cols.saturating_sub(1);
        }
        if self.alt_screen_cursor_row >= self.rows {
            self.alt_screen_cursor_row = self.rows.saturating_sub(1);
        }
        self.dirty = true;
    }

    // -----------------------------------------------------------------------
    // Byte processing
    // -----------------------------------------------------------------------

    pub fn process_byte(&mut self, b: u8) {
        // Any live output cancels a scrollback view so new text is visible.
        if self.scrollback.view_offset != 0 {
            self.scrollback.reset_view();
        }
        let action = self.parser.advance(b);
        self.execute_action(action);
    }

    fn execute_action(&mut self, action: VtAction) {
        match action {
            VtAction::Print(_) | VtAction::SetAttribute(_) | VtAction::Nop => {}
            _ => self.wrap_pending = false,
        }
        match action {
            VtAction::Print(cp) => self.print_codepoint(cp),
            VtAction::Execute(ctrl) => self.execute_control(ctrl),
            VtAction::MoveCursor { direction, count } => self.move_cursor(direction, count),
            VtAction::SetCursorPos { row, col } => self.set_cursor_pos(row, col),
            VtAction::EraseDisplay(mode) => self.erase_display(mode),
            VtAction::EraseLine(mode) => self.erase_line(mode),
            VtAction::ScrollUp(n) => self.scroll_up_n(n),
            VtAction::ScrollDown(n) => self.scroll_down_n(n),
            VtAction::SetAttribute(attr) => self.apply_sgr(attr),
            VtAction::SaveCursor => self.save_cursor(),
            VtAction::RestoreCursor => self.restore_cursor(),
            VtAction::SetScrollRegion { top, bottom } => self.set_scroll_region(top, bottom),
            VtAction::InsertLines(n) => self.insert_lines(n),
            VtAction::DeleteLines(n) => self.delete_lines(n),
            VtAction::DeleteChars(n) => self.delete_chars(n),
            VtAction::InsertChars(n) => self.insert_chars(n),
            VtAction::EraseChars(n) => self.erase_chars(n),
            VtAction::SetMode(mode) => self.set_dec_mode(mode),
            VtAction::ResetMode(mode) => self.reset_dec_mode(mode),
            VtAction::Nop => {}
        }
        self.dirty = true;
    }

    // -----------------------------------------------------------------------
    // Action handlers
    // -----------------------------------------------------------------------

    fn print_codepoint(&mut self, cp: u32) {
        if self.rows == 0 || self.cols == 0 {
            return;
        }

        // Commit a pending wrap before placing the next char.
        if self.wrap_pending {
            self.wrap_pending = false;
            if self.parser.auto_wrap {
                self.cursor_col = 0;
                self.advance_row();
            }
        }

        let wide = is_double_width(cp);

        // A wide char that cannot fit in the remaining columns wraps early
        // (autowrap) — there is no half-cell placement.
        if wide && self.cursor_col + 2 > self.cols && self.parser.auto_wrap {
            self.cursor_col = 0;
            self.advance_row();
        }

        let row = self.cursor_row as usize;
        let col = self.cursor_col as usize;
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        if row >= rows || col >= cols {
            return;
        }

        let cell_attr = CellAttributes {
            fg: self.cursor_attrs.effective_fg(),
            bg: self.cursor_attrs.effective_bg(),
        };

        let width: u16 = if wide && col + 1 < cols {
            self.cells.set(
                row,
                col,
                Cell {
                    codepoint: cp,
                    attrs: cell_attr,
                },
            );
            self.cells.set(
                row,
                col + 1,
                Cell {
                    codepoint: CONTINUATION_CODEPOINT,
                    attrs: cell_attr,
                },
            );
            2
        } else if wide {
            // No autowrap and no room for the pair: a placeholder space.
            self.cells.set(
                row,
                col,
                Cell {
                    codepoint: b' ' as u32,
                    attrs: cell_attr,
                },
            );
            1
        } else {
            self.cells.set(
                row,
                col,
                Cell {
                    codepoint: cp,
                    attrs: cell_attr,
                },
            );
            1
        };

        let end = self.cursor_col.saturating_add(width);
        if end >= self.cols {
            // Last column filled: stay put with the wrap pending. Without
            // autowrap the pending commit is a no-op and the last column
            // keeps being overwritten, matching xterm.
            self.cursor_col = self.cols - 1;
            self.wrap_pending = true;
        } else {
            self.cursor_col = end;
        }
    }

    fn execute_control(&mut self, ctrl: u8) {
        match ctrl {
            b'\n' | 0x0B | 0x0C => {
                self.wrap_pending = false;
                self.advance_row();
            }
            b'\r' => {
                self.wrap_pending = false;
                self.cursor_col = 0;
            }
            0x08 => {
                self.wrap_pending = false;
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                }
            }
            b'\t' => {
                // Tabs clamp at the last column; they never wrap (xterm).
                self.wrap_pending = false;
                let next = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next.min(self.cols.saturating_sub(1));
            }
            0x07 => {}
            _ => {}
        }
    }

    fn move_cursor(&mut self, direction: Direction, count: u16) {
        match direction {
            Direction::Up => {
                self.cursor_row = self.cursor_row.saturating_sub(count);
            }
            Direction::Down => {
                self.cursor_row = self.cursor_row.saturating_add(count);
                let max = self.rows.saturating_sub(1);
                if self.cursor_row > max {
                    self.cursor_row = max;
                }
            }
            Direction::Forward => {
                self.cursor_col = self.cursor_col.saturating_add(count);
                let max = self.cols.saturating_sub(1);
                if self.cursor_col > max {
                    self.cursor_col = max;
                }
            }
            Direction::Back => {
                self.cursor_col = self.cursor_col.saturating_sub(count);
            }
        }
    }

    fn set_cursor_pos(&mut self, row: u16, col: u16) {
        self.cursor_row = if row >= self.rows {
            self.rows.saturating_sub(1)
        } else {
            row
        };
        self.cursor_col = if col >= self.cols {
            self.cols.saturating_sub(1)
        } else {
            col
        };
    }

    fn erase_display(&mut self, mode: EraseMode) {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let cr = self.cursor_row as usize;
        let cc = self.cursor_col as usize;
        let clear_cell = self.blank_with_attrs();
        match mode {
            EraseMode::ToEnd => {
                if cr < rows {
                    for c in cc..cols {
                        self.cells.set(cr, c, clear_cell);
                    }
                }
                for r in (cr + 1)..rows {
                    self.clear_row_with_attr(r, clear_cell.attrs);
                }
            }
            EraseMode::ToStart => {
                for r in 0..cr {
                    self.clear_row_with_attr(r, clear_cell.attrs);
                }
                if cr < rows {
                    let end = if cc < cols { cc + 1 } else { cols };
                    for c in 0..end {
                        self.cells.set(cr, c, clear_cell);
                    }
                }
            }
            EraseMode::All => {
                for r in 0..rows {
                    self.clear_row_with_attr(r, clear_cell.attrs);
                }
                self.cursor_row = 0;
                self.cursor_col = 0;
            }
        }
    }

    fn erase_line(&mut self, mode: EraseMode) {
        let cols = self.cols as usize;
        let cr = self.cursor_row as usize;
        let cc = self.cursor_col as usize;
        if cr >= self.rows as usize {
            return;
        }
        let clear_cell = self.blank_with_attrs();
        match mode {
            EraseMode::ToEnd => {
                for c in cc..cols {
                    self.cells.set(cr, c, clear_cell);
                }
            }
            EraseMode::ToStart => {
                let end = if cc < cols { cc + 1 } else { cols };
                for c in 0..end {
                    self.cells.set(cr, c, clear_cell);
                }
            }
            EraseMode::All => {
                self.clear_row_with_attr(cr, clear_cell.attrs);
            }
        }
    }

    fn scroll_up_n(&mut self, n: u16) {
        for _ in 0..n {
            self.scroll_up();
        }
    }

    fn scroll_down_n(&mut self, n: u16) {
        let cols = self.cols as usize;
        let sr_top = self.scroll_top as usize;
        let sr_bottom = self.effective_scroll_bottom() as usize;
        if sr_bottom <= sr_top || cols == 0 {
            return;
        }
        let region_height = sr_bottom - sr_top + 1;
        let shift = core::cmp::min(n as usize, region_height);
        if shift < region_height {
            let mut row = sr_bottom;
            loop {
                if row < sr_top + shift {
                    break;
                }
                self.cells.row_copy(row, row - shift, cols);
                if row == sr_top + shift {
                    break;
                }
                row -= 1;
            }
        }
        for r in sr_top..sr_top + shift {
            if r <= sr_bottom {
                for c in 0..cols {
                    self.cells.set(r, c, Cell::blank());
                }
            }
        }
    }

    fn apply_sgr(&mut self, attr: SgrAttr) {
        match attr {
            SgrAttr::Reset => self.cursor_attrs = CursorAttributes::default_attrs(),
            SgrAttr::Bold => self.cursor_attrs.bold = true,
            SgrAttr::NoBold => self.cursor_attrs.bold = false,
            SgrAttr::Underline => self.cursor_attrs.underline = true,
            SgrAttr::NoUnderline => self.cursor_attrs.underline = false,
            SgrAttr::Inverse => self.cursor_attrs.inverse = true,
            SgrAttr::NoInverse => self.cursor_attrs.inverse = false,
            SgrAttr::ForegroundColor(n) => {
                if (n as usize) < 8 {
                    self.cursor_attrs.fg = ANSI_COLORS[n as usize];
                }
            }
            SgrAttr::BackgroundColor(n) => {
                if (n as usize) < 8 {
                    self.cursor_attrs.bg = ANSI_COLORS[n as usize];
                }
            }
            SgrAttr::BrightForeground(n) => {
                if (n as usize) < 8 {
                    self.cursor_attrs.fg = ANSI_BRIGHT_COLORS[n as usize];
                }
            }
            SgrAttr::BrightBackground(n) => {
                if (n as usize) < 8 {
                    self.cursor_attrs.bg = ANSI_BRIGHT_COLORS[n as usize];
                }
            }
            SgrAttr::DefaultForeground => self.cursor_attrs.fg = FG_COLOR,
            SgrAttr::DefaultBackground => self.cursor_attrs.bg = BG_COLOR,
            SgrAttr::Foreground256(n) => self.cursor_attrs.fg = color256_to_rgb(n),
            SgrAttr::Background256(n) => self.cursor_attrs.bg = color256_to_rgb(n),
            SgrAttr::ForegroundRgb(r, g, b) => {
                self.cursor_attrs.fg = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            }
            SgrAttr::BackgroundRgb(r, g, b) => {
                self.cursor_attrs.bg = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            }
        }
    }

    fn save_cursor(&mut self) {
        self.saved_cursor_row = self.cursor_row;
        self.saved_cursor_col = self.cursor_col;
        self.saved_cursor_attrs = self.cursor_attrs;
    }

    fn restore_cursor(&mut self) {
        self.cursor_row = if self.saved_cursor_row >= self.rows {
            self.rows.saturating_sub(1)
        } else {
            self.saved_cursor_row
        };
        self.cursor_col = if self.saved_cursor_col >= self.cols {
            self.cols.saturating_sub(1)
        } else {
            self.saved_cursor_col
        };
        self.cursor_attrs = self.saved_cursor_attrs;
    }

    fn set_dec_mode(&mut self, mode: u16) {
        match mode {
            25 => self.cursor_visible = true,
            1049 => self.enter_alt_screen(),
            2004 => self.bracketed_paste = true,
            _ => {}
        }
    }

    fn reset_dec_mode(&mut self, mode: u16) {
        match mode {
            25 => self.cursor_visible = false,
            1049 => self.leave_alt_screen(),
            2004 => self.bracketed_paste = false,
            _ => {}
        }
    }

    fn enter_alt_screen(&mut self) {
        if self.in_alt_screen {
            return;
        }
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        self.alt_cells.copy_from(&self.cells, rows, cols);
        self.alt_screen_cursor_row = self.cursor_row;
        self.alt_screen_cursor_col = self.cursor_col;
        self.cells.clear_all();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.in_alt_screen = true;
    }

    fn leave_alt_screen(&mut self) {
        if !self.in_alt_screen {
            return;
        }
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        self.cells.copy_from(&self.alt_cells, rows, cols);
        self.cursor_row = self.alt_screen_cursor_row;
        self.cursor_col = self.alt_screen_cursor_col;
        self.in_alt_screen = false;
    }

    fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        let max_row = self.rows.saturating_sub(1);
        let b = if bottom == 0 {
            max_row
        } else {
            bottom.saturating_sub(1).min(max_row)
        };
        let t = top.min(b);
        self.scroll_top = t;
        self.scroll_bottom = b;
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    fn insert_lines(&mut self, n: u16) {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let sr_top = self.scroll_top as usize;
        let sr_bottom = self.effective_scroll_bottom() as usize;
        let cur = self.cursor_row as usize;
        if cur < sr_top || cur > sr_bottom || rows == 0 || cols == 0 {
            return;
        }
        let shift = (n as usize).min(sr_bottom - cur + 1);
        let mut row = sr_bottom;
        while row >= cur + shift {
            self.cells.row_copy(row, row - shift, cols);
            if row == cur + shift {
                break;
            }
            row -= 1;
        }
        for r in cur..cur + shift {
            if r <= sr_bottom {
                for c in 0..cols {
                    self.cells.set(r, c, Cell::blank());
                }
            }
        }
    }

    fn delete_lines(&mut self, n: u16) {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let sr_top = self.scroll_top as usize;
        let sr_bottom = self.effective_scroll_bottom() as usize;
        let cur = self.cursor_row as usize;
        if cur < sr_top || cur > sr_bottom || rows == 0 || cols == 0 {
            return;
        }
        let shift = (n as usize).min(sr_bottom - cur + 1);
        for row in cur..=sr_bottom {
            let src = row + shift;
            if src <= sr_bottom {
                self.cells.row_copy(row, src, cols);
            } else {
                for c in 0..cols {
                    self.cells.set(row, c, Cell::blank());
                }
            }
        }
    }

    fn delete_chars(&mut self, n: u16) {
        let row = self.cursor_row as usize;
        let col = self.cursor_col as usize;
        let cols = self.cols as usize;
        if row >= self.rows as usize || col >= cols {
            return;
        }
        let shift = (n as usize).min(cols - col);
        for c in col..cols {
            let src = c + shift;
            let cell = if src < cols {
                self.cells.get(row, src)
            } else {
                Cell::blank()
            };
            self.cells.set(row, c, cell);
        }
    }

    fn insert_chars(&mut self, n: u16) {
        let row = self.cursor_row as usize;
        let col = self.cursor_col as usize;
        let cols = self.cols as usize;
        if row >= self.rows as usize || col >= cols {
            return;
        }
        let shift = (n as usize).min(cols - col);
        let mut c = cols;
        while c > col + shift {
            c -= 1;
            self.cells.set(row, c, self.cells.get(row, c - shift));
        }
        for c in col..col + shift {
            if c < cols {
                self.cells.set(row, c, Cell::blank());
            }
        }
    }

    fn erase_chars(&mut self, n: u16) {
        let row = self.cursor_row as usize;
        let col = self.cursor_col as usize;
        let cols = self.cols as usize;
        if row >= self.rows as usize || col >= cols {
            return;
        }
        let end = (col + n as usize).min(cols);
        let clear_cell = self.blank_with_attrs();
        for c in col..end {
            self.cells.set(row, c, clear_cell);
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn blank_with_attrs(&self) -> Cell {
        Cell {
            codepoint: b' ' as u32,
            attrs: CellAttributes {
                fg: self.cursor_attrs.effective_fg(),
                bg: self.cursor_attrs.effective_bg(),
            },
        }
    }

    fn effective_scroll_bottom(&self) -> u16 {
        if self.scroll_bottom == 0 || self.scroll_bottom >= self.rows {
            self.rows.saturating_sub(1)
        } else {
            self.scroll_bottom
        }
    }

    /// Move the cursor one row down, scrolling the region when it passes
    /// the bottom margin.
    fn advance_row(&mut self) {
        self.cursor_row = self.cursor_row.saturating_add(1);
        let sr_bottom = self.effective_scroll_bottom();
        if self.cursor_row > sr_bottom {
            let n = self.cursor_row - sr_bottom;
            for _ in 0..n {
                self.scroll_up();
            }
            self.cursor_row = sr_bottom;
        }
    }

    fn clear_row_with_attr(&mut self, row: usize, attr: CellAttributes) {
        if row >= self.rows as usize {
            return;
        }
        let cols = self.cols as usize;
        let clear_cell = Cell {
            codepoint: b' ' as u32,
            attrs: attr,
        };
        for c in 0..cols {
            self.cells.set(row, c, clear_cell);
        }
    }

    fn scroll_up(&mut self) {
        let cols = self.cols as usize;
        let sr_top = self.scroll_top as usize;
        let sr_bottom = self.effective_scroll_bottom() as usize;
        if sr_bottom <= sr_top || cols == 0 {
            return;
        }

        // Only a full-screen scroll (not a constrained scroll region, not the
        // alt screen) feeds the scrollback history — matches the kernel.
        let full_screen = sr_top == 0 && sr_bottom == (self.rows as usize).saturating_sub(1);
        if full_screen && !self.in_alt_screen {
            self.scrollback.push_row(&self.cells, sr_top, cols);
            self.scrollback.reset_view();
            // A line just became history: advance the absolute line origin so
            // selection anchors keep naming the same content.
            self.total_scrolled = self.total_scrolled.wrapping_add(1);
        }

        for row in (sr_top + 1)..=sr_bottom {
            self.cells.row_copy(row - 1, row, cols);
        }
        for c in 0..cols {
            self.cells.set(sr_bottom, c, Cell::blank());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph_at(g: &TerminalGrid, row: usize, col: usize) -> char {
        char::from_u32(g.cells.get(row, col).glyph()).unwrap_or('?')
    }

    fn feed(g: &mut TerminalGrid, bytes: &[u8]) {
        for &b in bytes {
            g.process_byte(b);
        }
    }

    #[test]
    fn prints_and_advances_cursor() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"hi");
        assert_eq!(glyph_at(&g, 0, 0), 'h');
        assert_eq!(glyph_at(&g, 0, 1), 'i');
        assert_eq!(g.cursor_col, 2);
        assert_eq!(g.cursor_row, 0);
    }

    #[test]
    fn cr_and_lf_independent() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"ab\r\nc");
        // \r returns to col 0, \n moves down one row.
        assert_eq!(glyph_at(&g, 0, 0), 'a');
        assert_eq!(glyph_at(&g, 1, 0), 'c');
        assert_eq!(g.cursor_row, 1);
        assert_eq!(g.cursor_col, 1);
    }

    #[test]
    fn backspace_moves_left_without_erasing() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"ab\x08");
        // BS only moves the cursor; ldisc does BS-space-BS to erase.
        assert_eq!(g.cursor_col, 1);
        assert_eq!(glyph_at(&g, 0, 1), 'b');
    }

    #[test]
    fn tab_advances_to_next_stop() {
        let mut g = TerminalGrid::new(5, 40);
        feed(&mut g, b"a\t");
        assert_eq!(g.cursor_col, 8);
    }

    #[test]
    fn wrap_to_next_line() {
        let mut g = TerminalGrid::new(5, 3);
        feed(&mut g, b"abcd");
        assert_eq!(glyph_at(&g, 0, 0), 'a');
        assert_eq!(glyph_at(&g, 0, 2), 'c');
        assert_eq!(glyph_at(&g, 1, 0), 'd');
        assert_eq!(g.cursor_row, 1);
    }

    #[test]
    fn exactly_full_line_defers_wrap() {
        let mut g = TerminalGrid::new(5, 3);
        // Fill row 0 exactly: the cursor stays on row 0 (wrap pending).
        feed(&mut g, b"abc");
        assert_eq!(g.cursor_row, 0);
        assert_eq!(g.cursor_col, 2);
        // An explicit CRLF after the full row advances exactly ONE row —
        // the pending wrap must not stack with the newline.
        feed(&mut g, b"\r\nx");
        assert_eq!(glyph_at(&g, 1, 0), 'x');
        assert_eq!(g.cursor_row, 1);
        assert_eq!(glyph_at(&g, 0, 2), 'c');
    }

    #[test]
    fn pending_wrap_commits_on_next_print() {
        let mut g = TerminalGrid::new(5, 3);
        feed(&mut g, b"abcx");
        assert_eq!(glyph_at(&g, 1, 0), 'x');
        assert_eq!(g.cursor_row, 1);
        assert_eq!(g.cursor_col, 1);
    }

    #[test]
    fn carriage_return_cancels_pending_wrap() {
        let mut g = TerminalGrid::new(5, 3);
        // Full row, then \r: overwriting from column 0 stays on row 0.
        feed(&mut g, b"abc\rX");
        assert_eq!(glyph_at(&g, 0, 0), 'X');
        assert_eq!(g.cursor_row, 0);
    }

    #[test]
    fn sgr_preserves_pending_wrap() {
        let mut g = TerminalGrid::new(5, 3);
        // SGR between the edge char and the next print must not eat the wrap.
        feed(&mut g, b"abc\x1b[31md");
        assert_eq!(glyph_at(&g, 1, 0), 'd');
        assert_eq!(g.cursor_row, 1);
    }

    #[test]
    fn scroll_pushes_to_scrollback() {
        let mut g = TerminalGrid::new(2, 4);
        // Three lines into a 2-row grid forces one scroll.
        feed(&mut g, b"L1\r\nL2\r\nL3");
        assert_eq!(glyph_at(&g, 0, 0), 'L');
        assert_eq!(glyph_at(&g, 0, 1), '2');
        assert_eq!(glyph_at(&g, 1, 0), 'L');
        assert_eq!(glyph_at(&g, 1, 1), '3');
        // Scrolling the view up should reveal L1.
        g.scroll_view_up(1);
        assert_eq!(g.viewing_history(), true);
        assert_eq!(char::from_u32(g.visible_cell(0, 0).glyph()).unwrap(), 'L');
        assert_eq!(char::from_u32(g.visible_cell(0, 1).glyph()).unwrap(), '1');
    }

    #[test]
    fn sgr_sets_foreground() {
        let mut g = TerminalGrid::new(3, 10);
        feed(&mut g, b"\x1b[31mR");
        let cell = g.cells.get(0, 0);
        assert_eq!(cell.attrs.fg, ANSI_COLORS[1]);
    }

    #[test]
    fn erase_display_all_clears() {
        let mut g = TerminalGrid::new(3, 5);
        feed(&mut g, b"xyz");
        feed(&mut g, b"\x1b[2J");
        assert_eq!(glyph_at(&g, 0, 0), ' ');
        assert_eq!(g.cursor_row, 0);
        assert_eq!(g.cursor_col, 0);
    }

    #[test]
    fn cursor_position_csi() {
        let mut g = TerminalGrid::new(10, 20);
        feed(&mut g, b"\x1b[5;7H");
        assert_eq!(g.cursor_row, 4);
        assert_eq!(g.cursor_col, 6);
    }

    #[test]
    fn color256_cube() {
        // index 196 = pure red corner of the 6x6x6 cube.
        assert_eq!(color256_to_rgb(196), 0x00FF_0000);
    }

    #[test]
    fn resize_preserves_content() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"hello");
        g.resize(8, 20);
        assert_eq!(glyph_at(&g, 0, 0), 'h');
        assert_eq!(glyph_at(&g, 0, 4), 'o');
        assert_eq!(g.rows, 8);
        assert_eq!(g.cols, 20);
    }

    #[test]
    fn resize_height_shrink_anchors_cursor_not_top() {
        let mut g = TerminalGrid::new(5, 10);
        // Five rows of content, cursor resting on the bottom row.
        feed(&mut g, b"a\r\nb\r\nc\r\nd\r\ne");
        assert_eq!(g.cursor_row, 4);
        g.resize(3, 10);
        // The cursor's line stays on screen at the new bottom; the two
        // displaced top rows scrolled into history instead of being clipped.
        assert_eq!(g.cursor_row, 2);
        assert_eq!(glyph_at(&g, 0, 0), 'c');
        assert_eq!(glyph_at(&g, 1, 0), 'd');
        assert_eq!(glyph_at(&g, 2, 0), 'e');
        assert_eq!(g.scrollback.count, 2);
    }

    #[test]
    fn resize_height_shrink_above_cursor_clips_bottom() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"a\r\nb");
        assert_eq!(g.cursor_row, 1);
        g.resize(3, 10);
        // Cursor already fits: plain top-aligned clip, nothing scrolled.
        assert_eq!(g.cursor_row, 1);
        assert_eq!(glyph_at(&g, 0, 0), 'a');
        assert_eq!(glyph_at(&g, 1, 0), 'b');
        assert_eq!(g.scrollback.count, 0);
    }

    #[test]
    fn resize_in_alt_screen_anchors_saved_main_cursor() {
        let mut g = TerminalGrid::new(5, 10);
        // Main cursor parked on the bottom row, then enter the alt screen
        // and move its cursor to the bottom too.
        feed(&mut g, b"a\r\nb\r\nc\r\nd\r\ne");
        feed(&mut g, b"\x1b[?1049h\x1b[5;1HALT");
        g.resize(3, 10);
        // Alt screen has no history: its cursor clamps.
        assert_eq!(g.cursor_row, 2);
        // Leaving alt restores a bottom-anchored main screen whose saved
        // cursor moved with its content.
        feed(&mut g, b"\x1b[?1049l");
        assert_eq!(g.cursor_row, 2);
        assert_eq!(glyph_at(&g, 2, 0), 'e');
        assert_eq!(g.scrollback.count, 2);
    }

    #[test]
    fn alt_screen_saves_and_restores_main() {
        let mut g = TerminalGrid::new(3, 10);
        feed(&mut g, b"main\x1b[2;3H");
        // Enter: alt screen starts blank with the cursor homed.
        feed(&mut g, b"\x1b[?1049h");
        assert_eq!(glyph_at(&g, 0, 0), ' ');
        assert_eq!((g.cursor_row, g.cursor_col), (0, 0));
        feed(&mut g, b"ALT");
        assert_eq!(glyph_at(&g, 0, 0), 'A');
        // Leave: main content AND cursor come back exactly.
        feed(&mut g, b"\x1b[?1049l");
        assert_eq!(glyph_at(&g, 0, 0), 'm');
        assert_eq!(glyph_at(&g, 0, 3), 'n');
        assert_eq!((g.cursor_row, g.cursor_col), (1, 2));
    }

    #[test]
    fn alt_screen_enter_twice_keeps_saved_main() {
        let mut g = TerminalGrid::new(3, 10);
        feed(&mut g, b"keep\x1b[?1049h\x1b[?1049hALT\x1b[?1049l");
        // The second 1049h must not overwrite the saved main screen with
        // the (blank) alt contents.
        assert_eq!(glyph_at(&g, 0, 0), 'k');
    }

    #[test]
    fn alt_screen_does_not_feed_scrollback() {
        let mut g = TerminalGrid::new(2, 4);
        feed(&mut g, b"\x1b[?1049h");
        // Scroll three lines through the 2-row alt screen.
        feed(&mut g, b"A1\r\nA2\r\nA3");
        feed(&mut g, b"\x1b[?1049l");
        // Nothing from the alt screen may appear in history.
        g.scroll_view_up(1);
        assert!(!g.viewing_history());
    }

    #[test]
    fn width_resize_grows_scrollback_at_most_once() {
        // A drag re-strides the ring for every cell-width step. The backing
        // grows once to its ceiling and is reused thereafter, so no step past
        // the widest point reallocates this multi-megabyte ring.
        let mut g = TerminalGrid::new(24, 80);
        assert_eq!(g.scrollback.buf.len(), SCROLLBACK_LINES * 80);
        // Reach the maximum width, then capture the now-ceiling backing.
        g.resize(24, MAX_COLS as u16);
        assert_eq!(g.scrollback.buf.len(), SCROLLBACK_LINES * MAX_COLS);
        let cap = g.scrollback.buf.capacity();
        let ptr = g.scrollback.buf.as_ptr();
        for cols in (10..=MAX_COLS as u16).chain((10..=MAX_COLS as u16).rev()) {
            g.resize(24, cols);
            assert_eq!(g.scrollback.cols, cols as usize);
            assert_eq!(
                g.scrollback.buf.capacity(),
                cap,
                "post-ceiling resize reallocated the scrollback ring"
            );
            assert_eq!(
                g.scrollback.buf.as_ptr(),
                ptr,
                "post-ceiling resize moved the scrollback backing"
            );
        }
        // The re-strided ring still records history: scroll a marker off the
        // 24-row grid and read it back.
        feed(&mut g, b"\x1b[H top");
        feed(&mut g, &[b'\r', b'\n'].repeat(24));
        g.scroll_view_up(1);
        assert!(g.viewing_history());
        assert_eq!(char::from_u32(g.visible_cell(0, 1).glyph()).unwrap(), 't');
        assert_eq!(char::from_u32(g.visible_cell(0, 2).glyph()).unwrap(), 'o');
        assert_eq!(char::from_u32(g.visible_cell(0, 3).glyph()).unwrap(), 'p');
    }

    #[test]
    fn resize_drag_does_not_reallocate_cell_grids() {
        // The screen and alt grids are re-shaped by reusing pooled backings, so
        // a drag never reallocates them either (the residual churn class). Once
        // every backing has reached the ceiling capacity it must stay put.
        let mut g = TerminalGrid::new(100, 240);
        feed(&mut g, b"anchor");
        // Warm every pooled backing to its ceiling capacity.
        g.resize(40, 100);
        g.resize(100, 240);
        let max = MAX_ROWS * MAX_COLS;
        let caps = |g: &TerminalGrid| {
            (
                g.cells.cells.capacity(),
                g.alt_cells.cells.capacity(),
                g.resize_scratch.cells.capacity(),
            )
        };
        assert_eq!(caps(&g), (max, max, max));
        for rows in (10..=100u16).step_by(7) {
            for cols in (10..=240u16).step_by(11) {
                g.resize(rows, cols);
                assert_eq!(
                    caps(&g),
                    (max, max, max),
                    "resize reallocated a cell grid at {rows}x{cols}"
                );
            }
        }
        // Content within the overlap survives the churn.
        g.resize(100, 240);
        assert_eq!(glyph_at(&g, 0, 0), 'a');
        assert_eq!(glyph_at(&g, 0, 5), 'r');
    }

    #[test]
    fn bracketed_paste_mode_tracks_decset_2004() {
        let mut g = TerminalGrid::new(3, 10);
        assert!(!g.bracketed_paste());
        feed(&mut g, b"\x1b[?2004h");
        assert!(g.bracketed_paste());
        feed(&mut g, b"\x1b[?2004l");
        assert!(!g.bracketed_paste());
    }
}
