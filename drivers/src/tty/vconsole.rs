//! VConsole - framebuffer-backed virtual console text renderer.
//!
//! Manages cursor position, cell buffer, and direct framebuffer rendering
//! for TTY 1 (the virtual console). When no framebuffer is registered
//! (early boot or headless), output falls back to serial mirroring.
//!
//! Phase 40: VT100/ANSI terminal emulation — each output byte passes through
//! `VtParser`; typed `VtAction` variants drive cursor movement, erase, scroll,
//! and SGR color/attribute rendering.

use core::ptr;

use slopos_abi::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, get_glyph_or_space};
use slopos_lib::IrqMutex;

use crate::serial::serial_putc_com1;

use super::vtparser::{Direction, EraseMode, SgrAttr, VtAction, VtParser};

pub(crate) const VCONSOLE_MAX_COLS: usize = 240;
pub(crate) const VCONSOLE_MAX_ROWS: usize = 80;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 25;
const FG_COLOR: u32 = 0x00AAAAAA;
const BG_COLOR: u32 = 0x00000000;

// ---------------------------------------------------------------------------
// ANSI color tables (standard 8 + bright 8)
// ---------------------------------------------------------------------------

const ANSI_COLORS: [u32; 8] = [
    0x00000000, // Black
    0x00AA0000, // Red
    0x0000AA00, // Green
    0x00AA5500, // Yellow / Brown
    0x000000AA, // Blue
    0x00AA00AA, // Magenta
    0x0000AAAA, // Cyan
    0x00AAAAAA, // White
];

const ANSI_BRIGHT_COLORS: [u32; 8] = [
    0x00555555, // Bright Black  (Gray)
    0x00FF5555, // Bright Red
    0x0055FF55, // Bright Green
    0x00FFFF55, // Bright Yellow
    0x005555FF, // Bright Blue
    0x00FF55FF, // Bright Magenta
    0x0055FFFF, // Bright Cyan
    0x00FFFFFF, // Bright White
];

// ---------------------------------------------------------------------------
// Per-cell and cursor attribute types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) struct CellAttributes {
    pub(crate) fg: u32,
    pub(crate) bg: u32,
}

impl CellAttributes {
    const fn default_colors() -> Self {
        Self {
            fg: FG_COLOR,
            bg: BG_COLOR,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CursorAttributes {
    pub(crate) fg: u32,
    pub(crate) bg: u32,
    pub(crate) bold: bool,
    pub(crate) underline: bool,
    pub(crate) inverse: bool,
}

impl CursorAttributes {
    const fn default_attrs() -> Self {
        Self {
            fg: FG_COLOR,
            bg: BG_COLOR,
            bold: false,
            underline: false,
            inverse: false,
        }
    }

    /// Effective foreground color (handles bold → bright and inverse swap).
    fn effective_fg(&self) -> u32 {
        if self.inverse {
            self.bg
        } else if self.bold {
            // Promote standard colors to bright when bold is active.
            self.brighten(self.fg)
        } else {
            self.fg
        }
    }

    /// Effective background color (handles inverse swap).
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

    /// If color matches a standard ANSI color, return its bright variant.
    fn brighten(&self, color: u32) -> u32 {
        let mut i = 0;
        while i < 8 {
            if ANSI_COLORS[i] == color {
                return ANSI_BRIGHT_COLORS[i];
            }
            i += 1;
        }
        color
    }
}

// ---------------------------------------------------------------------------
// Framebuffer info
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) struct VConsoleFbInfo {
    pub(crate) base: *mut u8,
    pub(crate) pitch: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bytes_per_pixel: u8,
}

unsafe impl Send for VConsoleFbInfo {}

// ---------------------------------------------------------------------------
// VConsole state
// ---------------------------------------------------------------------------

pub(crate) struct VConsoleState {
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) fb: Option<VConsoleFbInfo>,
    pub(crate) cells: [[u8; VCONSOLE_MAX_COLS]; VCONSOLE_MAX_ROWS],
    // Phase 40: VT parser + per-cell color attributes
    pub(crate) parser: VtParser,
    pub(crate) cell_attrs: [[CellAttributes; VCONSOLE_MAX_COLS]; VCONSOLE_MAX_ROWS],
    pub(crate) cursor_attrs: CursorAttributes,
    pub(crate) saved_cursor_row: u16,
    pub(crate) saved_cursor_col: u16,
    pub(crate) saved_cursor_attrs: CursorAttributes,
    pub(crate) cursor_visible: bool,
    pub(crate) alt_screen_cells: [[u8; VCONSOLE_MAX_COLS]; VCONSOLE_MAX_ROWS],
    pub(crate) alt_screen_attrs: [[CellAttributes; VCONSOLE_MAX_COLS]; VCONSOLE_MAX_ROWS],
    pub(crate) alt_screen_cursor_row: u16,
    pub(crate) alt_screen_cursor_col: u16,
    pub(crate) in_alt_screen: bool,
}

impl VConsoleState {
    pub(crate) const fn new() -> Self {
        Self {
            cursor_row: 0,
            cursor_col: 0,
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            fb: None,
            cells: [[b' '; VCONSOLE_MAX_COLS]; VCONSOLE_MAX_ROWS],
            parser: VtParser::new(),
            cell_attrs: [[CellAttributes::default_colors(); VCONSOLE_MAX_COLS]; VCONSOLE_MAX_ROWS],
            cursor_attrs: CursorAttributes::default_attrs(),
            saved_cursor_row: 0,
            saved_cursor_col: 0,
            saved_cursor_attrs: CursorAttributes::default_attrs(),
            cursor_visible: true,
            alt_screen_cells: [[b' '; VCONSOLE_MAX_COLS]; VCONSOLE_MAX_ROWS],
            alt_screen_attrs: [[CellAttributes::default_colors(); VCONSOLE_MAX_COLS];
                VCONSOLE_MAX_ROWS],
            alt_screen_cursor_row: 0,
            alt_screen_cursor_col: 0,
            in_alt_screen: false,
        }
    }

    // -----------------------------------------------------------------------
    // Phase 40: VT-parser-driven byte processing
    // -----------------------------------------------------------------------

    /// Feed one byte through the VT parser and execute the resulting action.
    pub(crate) fn process_byte(&mut self, b: u8) {
        let action = self.parser.advance(b);
        self.execute_action(action);
    }

    fn execute_action(&mut self, action: VtAction) {
        match action {
            VtAction::Print(ch) => self.print_char(ch),
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
            VtAction::SetMode(mode) => self.set_dec_mode(mode),
            VtAction::ResetMode(mode) => self.reset_dec_mode(mode),
            VtAction::Nop => {}
        }
    }

    // -----------------------------------------------------------------------
    // Action handlers
    // -----------------------------------------------------------------------

    fn print_char(&mut self, ch: u8) {
        let row = self.cursor_row as usize;
        let col = self.cursor_col as usize;
        if row < self.rows as usize && col < self.cols as usize {
            self.cells[row][col] = ch;
            self.cell_attrs[row][col] = CellAttributes {
                fg: self.cursor_attrs.effective_fg(),
                bg: self.cursor_attrs.effective_bg(),
            };
            self.render_cell(self.cursor_row, self.cursor_col);
            self.cursor_col = self.cursor_col.saturating_add(1);
        }
        self.check_wrap_and_scroll();
    }

    fn execute_control(&mut self, ctrl: u8) {
        match ctrl {
            b'\n' | 0x0B | 0x0C => {
                // LF, VT, FF — all advance one line.
                self.cursor_row = self.cursor_row.saturating_add(1);
                self.cursor_col = 0;
            }
            b'\r' => {
                self.cursor_col = 0;
            }
            0x08 => {
                // BS
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                    let row = self.cursor_row as usize;
                    let col = self.cursor_col as usize;
                    if row < self.rows as usize && col < self.cols as usize {
                        self.cells[row][col] = b' ';
                        self.cell_attrs[row][col] = CellAttributes::default_colors();
                        self.render_cell(self.cursor_row, self.cursor_col);
                    }
                }
            }
            b'\t' => {
                let next = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next;
            }
            0x07 => {
                // BEL — no-op for now (no speaker).
            }
            _ => {}
        }
        self.check_wrap_and_scroll();
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
        let clear_attr = CellAttributes {
            fg: self.cursor_attrs.effective_fg(),
            bg: self.cursor_attrs.effective_bg(),
        };

        match mode {
            EraseMode::ToEnd => {
                // Cursor to end of line.
                if cr < rows {
                    for c in cc..cols {
                        self.cells[cr][c] = b' ';
                        self.cell_attrs[cr][c] = clear_attr;
                    }
                    self.render_row_range(cr, cc, cols);
                }
                // All lines below.
                for r in (cr + 1)..rows {
                    self.clear_row_with_attr(r, clear_attr);
                }
            }
            EraseMode::ToStart => {
                // All lines above.
                for r in 0..cr {
                    self.clear_row_with_attr(r, clear_attr);
                }
                // Start of line to cursor.
                if cr < rows {
                    let end = if cc < cols { cc + 1 } else { cols };
                    for c in 0..end {
                        self.cells[cr][c] = b' ';
                        self.cell_attrs[cr][c] = clear_attr;
                    }
                    self.render_row_range(cr, 0, end);
                }
            }
            EraseMode::All => {
                for r in 0..rows {
                    self.clear_row_with_attr(r, clear_attr);
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
        let clear_attr = CellAttributes {
            fg: self.cursor_attrs.effective_fg(),
            bg: self.cursor_attrs.effective_bg(),
        };

        match mode {
            EraseMode::ToEnd => {
                for c in cc..cols {
                    self.cells[cr][c] = b' ';
                    self.cell_attrs[cr][c] = clear_attr;
                }
                self.render_row_range(cr, cc, cols);
            }
            EraseMode::ToStart => {
                let end = if cc < cols { cc + 1 } else { cols };
                for c in 0..end {
                    self.cells[cr][c] = b' ';
                    self.cell_attrs[cr][c] = clear_attr;
                }
                self.render_row_range(cr, 0, end);
            }
            EraseMode::All => {
                self.clear_row_with_attr(cr, clear_attr);
            }
        }
    }

    fn scroll_up_n(&mut self, n: u16) {
        for _ in 0..n {
            self.scroll_up();
        }
    }

    fn scroll_down_n(&mut self, n: u16) {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        if rows == 0 || cols == 0 {
            return;
        }
        let shift = core::cmp::min(n as usize, rows);

        // Shift rows downward (iterate from bottom to avoid overwrite).
        // We must copy through a temp buffer to satisfy the borrow checker.
        if shift < rows {
            let mut row = rows - 1;
            loop {
                if row < shift {
                    break;
                }
                let src = row - shift;
                // Copy cells through stack buffer.
                let mut tmp_cells = [b' '; VCONSOLE_MAX_COLS];
                tmp_cells[..cols].copy_from_slice(&self.cells[src][..cols]);
                self.cells[row][..cols].copy_from_slice(&tmp_cells[..cols]);
                // Copy attrs through stack buffer.
                let mut tmp_attrs = [CellAttributes::default_colors(); VCONSOLE_MAX_COLS];
                tmp_attrs[..cols].copy_from_slice(&self.cell_attrs[src][..cols]);
                self.cell_attrs[row][..cols].copy_from_slice(&tmp_attrs[..cols]);
                if row == shift {
                    break;
                }
                row -= 1;
            }
        }

        // Clear the top `shift` rows.
        for r in 0..shift {
            self.cells[r][..cols].fill(b' ');
            for c in 0..cols {
                self.cell_attrs[r][c] = CellAttributes::default_colors();
            }
        }

        // Re-render entire screen.
        for r in 0..rows {
            for c in 0..cols {
                self.render_cell(r as u16, c as u16);
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
            _ => {}
        }
    }

    fn reset_dec_mode(&mut self, mode: u16) {
        match mode {
            25 => self.cursor_visible = false,
            1049 => self.leave_alt_screen(),
            _ => {}
        }
    }

    fn enter_alt_screen(&mut self) {
        if self.in_alt_screen {
            return;
        }
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        // Save main screen.
        for r in 0..rows {
            self.alt_screen_cells[r][..cols].copy_from_slice(&self.cells[r][..cols]);
            self.alt_screen_attrs[r][..cols].copy_from_slice(&self.cell_attrs[r][..cols]);
        }
        self.alt_screen_cursor_row = self.cursor_row;
        self.alt_screen_cursor_col = self.cursor_col;
        // Clear screen.
        for r in 0..rows {
            self.cells[r][..cols].fill(b' ');
            for c in 0..cols {
                self.cell_attrs[r][c] = CellAttributes::default_colors();
            }
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.in_alt_screen = true;
        // Re-render.
        for r in 0..rows {
            for c in 0..cols {
                self.render_cell(r as u16, c as u16);
            }
        }
    }

    fn leave_alt_screen(&mut self) {
        if !self.in_alt_screen {
            return;
        }
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        // Restore main screen.
        for r in 0..rows {
            self.cells[r][..cols].copy_from_slice(&self.alt_screen_cells[r][..cols]);
            self.cell_attrs[r][..cols].copy_from_slice(&self.alt_screen_attrs[r][..cols]);
        }
        self.cursor_row = self.alt_screen_cursor_row;
        self.cursor_col = self.alt_screen_cursor_col;
        self.in_alt_screen = false;
        // Re-render.
        for r in 0..rows {
            for c in 0..cols {
                self.render_cell(r as u16, c as u16);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn check_wrap_and_scroll(&mut self) {
        if self.cursor_col >= self.cols {
            self.cursor_col = 0;
            self.cursor_row = self.cursor_row.saturating_add(1);
        }
        if self.cursor_row >= self.rows {
            self.scroll_up();
            self.cursor_row = self.rows.saturating_sub(1);
        }
    }

    fn clear_row_with_attr(&mut self, row: usize, attr: CellAttributes) {
        if row >= self.rows as usize {
            return;
        }
        let cols = self.cols as usize;
        self.cells[row][..cols].fill(b' ');
        for c in 0..cols {
            self.cell_attrs[row][c] = attr;
        }
        for c in 0..cols {
            self.render_cell(row as u16, c as u16);
        }
    }

    fn render_row_range(&self, row: usize, col_start: usize, col_end: usize) {
        for c in col_start..col_end {
            self.render_cell(row as u16, c as u16);
        }
    }

    // -----------------------------------------------------------------------
    // Simple byte-level output (used by integration tests)
    // -----------------------------------------------------------------------

    #[allow(dead_code)] // Called by tty_tests — not visible to the lib build.
    pub(crate) fn write_byte(&mut self, b: u8) {
        match b {
            b'\n' => {
                self.cursor_row = self.cursor_row.saturating_add(1);
                self.cursor_col = 0;
            }
            b'\r' => {
                self.cursor_col = 0;
            }
            b'\x08' => {
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                    let row = self.cursor_row as usize;
                    let col = self.cursor_col as usize;
                    if row < self.rows as usize && col < self.cols as usize {
                        self.cells[row][col] = b' ';
                        self.cell_attrs[row][col] = CellAttributes::default_colors();
                        self.render_cell(self.cursor_row, self.cursor_col);
                    }
                }
            }
            b'\t' => {
                let next = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next;
            }
            0x20..=0x7E => {
                let row = self.cursor_row as usize;
                let col = self.cursor_col as usize;
                if row < self.rows as usize && col < self.cols as usize {
                    self.cells[row][col] = b;
                    self.cell_attrs[row][col] = CellAttributes {
                        fg: self.cursor_attrs.effective_fg(),
                        bg: self.cursor_attrs.effective_bg(),
                    };
                    self.render_cell(self.cursor_row, self.cursor_col);
                    self.cursor_col = self.cursor_col.saturating_add(1);
                }
            }
            _ => {}
        }

        self.check_wrap_and_scroll();
    }

    // -----------------------------------------------------------------------
    // Scroll / render primitives
    // -----------------------------------------------------------------------

    pub(crate) fn scroll_up(&mut self) {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        if rows == 0 || cols == 0 {
            return;
        }

        for row in 1..rows {
            let (head, tail) = self.cells.split_at_mut(row);
            head[row - 1][..cols].copy_from_slice(&tail[0][..cols]);
        }
        self.cells[rows - 1][..cols].fill(b' ');

        // Shift cell_attrs the same way.
        for row in 1..rows {
            let (head, tail) = self.cell_attrs.split_at_mut(row);
            head[row - 1][..cols].copy_from_slice(&tail[0][..cols]);
        }
        for c in 0..cols {
            self.cell_attrs[rows - 1][c] = CellAttributes::default_colors();
        }

        if let Some(fb) = self.fb {
            let row_px = FONT_CHAR_HEIGHT as usize;
            let pitch = fb.pitch as usize;
            let copy_rows = rows.saturating_sub(1).saturating_mul(row_px);
            let copy_bytes = copy_rows.saturating_mul(pitch);
            if copy_bytes > 0 {
                unsafe {
                    let src = fb.base.add(row_px.saturating_mul(pitch));
                    ptr::copy(src, fb.base, copy_bytes);
                }
            }

            let clear_start_y = rows.saturating_sub(1).saturating_mul(row_px);
            let width = fb.width as usize;
            for y in clear_start_y..clear_start_y.saturating_add(row_px) {
                for x in 0..width {
                    self.put_pixel(&fb, x, y, BG_COLOR);
                }
            }
        }
    }

    pub(crate) fn render_cell(&self, row: u16, col: u16) {
        let Some(fb) = self.fb else {
            return;
        };

        let row_usize = row as usize;
        let col_usize = col as usize;
        if row_usize >= self.rows as usize || col_usize >= self.cols as usize {
            return;
        }

        let attrs = &self.cell_attrs[row_usize][col_usize];
        let glyph = get_glyph_or_space(self.cells[row_usize][col_usize]);
        let x0 = col_usize.saturating_mul(FONT_CHAR_WIDTH as usize);
        let y0 = row_usize.saturating_mul(FONT_CHAR_HEIGHT as usize);

        for gy in 0..FONT_CHAR_HEIGHT as usize {
            let bits = glyph[gy];
            for gx in 0..FONT_CHAR_WIDTH as usize {
                let mask = 1u8 << (7 - gx as u8);
                let color = if (bits & mask) != 0 {
                    attrs.fg
                } else {
                    attrs.bg
                };
                self.put_pixel(&fb, x0 + gx, y0 + gy, color);
            }
        }
    }

    pub(crate) fn clear_row(&mut self, row: u16) {
        let row_usize = row as usize;
        if row_usize >= self.rows as usize {
            return;
        }
        let cols = self.cols as usize;
        self.cells[row_usize][..cols].fill(b' ');
        for c in 0..cols {
            self.cell_attrs[row_usize][c] = CellAttributes::default_colors();
        }
        for col in 0..cols {
            self.render_cell(row, col as u16);
        }
    }

    pub(crate) fn recalculate_dimensions(&mut self) {
        if let Some(fb) = self.fb {
            let char_w = FONT_CHAR_WIDTH as u32;
            let char_h = FONT_CHAR_HEIGHT as u32;

            let calc_cols = (fb.width / char_w).max(1) as usize;
            let calc_rows = (fb.height / char_h).max(1) as usize;

            self.cols = core::cmp::min(calc_cols, VCONSOLE_MAX_COLS) as u16;
            self.rows = core::cmp::min(calc_rows, VCONSOLE_MAX_ROWS) as u16;
        } else {
            self.cols = DEFAULT_COLS;
            self.rows = DEFAULT_ROWS;
        }

        if self.cursor_col >= self.cols {
            self.cursor_col = self.cols.saturating_sub(1);
        }
        if self.cursor_row >= self.rows {
            self.cursor_row = self.rows.saturating_sub(1);
        }
    }

    fn put_pixel(&self, fb: &VConsoleFbInfo, x: usize, y: usize, color: u32) {
        if x >= fb.width as usize || y >= fb.height as usize {
            return;
        }

        let bpp = fb.bytes_per_pixel as usize;
        if bpp == 0 {
            return;
        }
        let offset = y
            .saturating_mul(fb.pitch as usize)
            .saturating_add(x.saturating_mul(bpp));

        unsafe {
            let p = fb.base.add(offset);
            match fb.bytes_per_pixel {
                4 => {
                    ptr::write(p, (color & 0xFF) as u8);
                    ptr::write(p.add(1), ((color >> 8) & 0xFF) as u8);
                    ptr::write(p.add(2), ((color >> 16) & 0xFF) as u8);
                    ptr::write(p.add(3), 0);
                }
                3 => {
                    ptr::write(p, (color & 0xFF) as u8);
                    ptr::write(p.add(1), ((color >> 8) & 0xFF) as u8);
                    ptr::write(p.add(2), ((color >> 16) & 0xFF) as u8);
                }
                2 => {
                    let r = ((color >> 16) & 0xFF) as u16;
                    let g = ((color >> 8) & 0xFF) as u16;
                    let b = (color & 0xFF) as u16;
                    let rgb565 = ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3);
                    ptr::write(p, (rgb565 & 0xFF) as u8);
                    ptr::write(p.add(1), ((rgb565 >> 8) & 0xFF) as u8);
                }
                _ => {}
            }
        }
    }
}

static VCONSOLE_STATE: IrqMutex<VConsoleState> = IrqMutex::new(VConsoleState::new());

pub fn register_framebuffer(
    base: *mut u8,
    pitch: u32,
    width: u32,
    height: u32,
    bytes_per_pixel: u8,
) {
    if base.is_null() || pitch == 0 || width == 0 || height == 0 || bytes_per_pixel == 0 {
        return;
    }

    let mut state = VCONSOLE_STATE.lock();
    state.fb = Some(VConsoleFbInfo {
        base,
        pitch,
        width,
        height,
        bytes_per_pixel,
    });
    state.recalculate_dimensions();
    for row in 0..state.rows {
        state.clear_row(row);
    }
    state.cursor_row = 0;
    state.cursor_col = 0;
}

pub fn write(data: &[u8]) {
    let mut state = VCONSOLE_STATE.lock();
    if state.fb.is_none() {
        for &b in data {
            serial_putc_com1(b);
        }
        return;
    }

    for &b in data {
        state.process_byte(b);
    }
}

pub fn has_framebuffer() -> bool {
    VCONSOLE_STATE.lock().fb.is_some()
}

#[cfg(feature = "itests")]
pub(crate) fn reset_for_tests() {
    let mut state = VCONSOLE_STATE.lock();
    state.cursor_row = 0;
    state.cursor_col = 0;
    state.rows = DEFAULT_ROWS;
    state.cols = DEFAULT_COLS;
    state.fb = None;
    for r in 0..VCONSOLE_MAX_ROWS {
        state.cells[r].fill(b' ');
        for c in 0..VCONSOLE_MAX_COLS {
            state.cell_attrs[r][c] = CellAttributes::default_colors();
        }
    }
    state.parser = VtParser::new();
    state.cursor_attrs = CursorAttributes::default_attrs();
    state.saved_cursor_row = 0;
    state.saved_cursor_col = 0;
    state.saved_cursor_attrs = CursorAttributes::default_attrs();
    state.cursor_visible = true;
    for r in 0..VCONSOLE_MAX_ROWS {
        state.alt_screen_cells[r].fill(b' ');
        for c in 0..VCONSOLE_MAX_COLS {
            state.alt_screen_attrs[r][c] = CellAttributes::default_colors();
        }
    }
    state.alt_screen_cursor_row = 0;
    state.alt_screen_cursor_col = 0;
    state.in_alt_screen = false;
}
