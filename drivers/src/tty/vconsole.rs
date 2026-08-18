//! VConsole - framebuffer-backed virtual console text renderer.
//!
//! Manages cursor position, cell buffer, VT100/ANSI emulation (each byte
//! passes through `VtParser`) and direct framebuffer rendering for TTY 1.
//! When no framebuffer is registered (early boot or headless), output falls
//! back to serial mirroring.

use core::sync::atomic::{AtomicBool, Ordering};
use slopos_ostd::lock_class;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, init_struct_with};
use slopos_ostd::{KBox, KVec, write_field};

use slopos_abi::unicode::is_double_width;
use slopos_font::atlas::{self, blend_coverage_u32};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use super::vtparser::{Direction, EraseMode, SgrAttr, VtAction, VtParser};

pub(crate) const VCONSOLE_MAX_COLS: usize = 240;
pub(crate) const VCONSOLE_MAX_ROWS: usize = 80;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 25;
const FG_COLOR: u32 = 0x00AAAAAA;
const BG_COLOR: u32 = 0x00000000;

const SCROLLBACK_LINES: usize = 200;

/// Continuation marker for the right half of a double-width character.
const CONTINUATION_CODEPOINT: u32 = 0xFFFF_FFFF;

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
pub(crate) struct Cell {
    pub(crate) codepoint: u32,
    pub(crate) attrs: CellAttributes,
}

impl Cell {
    const fn blank() -> Self {
        Self {
            codepoint: b' ' as u32,
            attrs: CellAttributes::default_colors(),
        }
    }
}

pub(crate) struct CellGrid {
    cells: Option<KVec<Cell>>,
    cols: usize,
}

impl CellGrid {
    pub(crate) const fn empty() -> Self {
        Self {
            cells: None,
            cols: 0,
        }
    }

    /// Size the grid in place. Production sizing allocates outside the console
    /// lock and hands the buffer over via [`Self::adopt`].
    #[cfg(feature = "test-hooks")]
    pub(crate) fn allocate(&mut self, rows: usize, cols: usize) {
        let total = rows.saturating_mul(cols);
        if total == 0 {
            self.cells = None;
            self.cols = 0;
            return;
        }
        if let Some(ref existing) = self.cells {
            if existing.len() == total && self.cols == cols {
                self.clear_all();
                return;
            }
        }
        self.cells = KVec::filled(Cell::blank(), total).ok();
        self.cols = cols;
    }

    #[inline]
    pub(crate) fn get(&self, row: usize, col: usize) -> Cell {
        match self.cells.as_ref() {
            Some(c) => {
                let idx = row * self.cols + col;
                if idx < c.len() { c[idx] } else { Cell::blank() }
            }
            None => Cell::blank(),
        }
    }

    #[inline]
    pub(crate) fn set(&mut self, row: usize, col: usize, cell: Cell) {
        if let Some(ref mut c) = self.cells {
            let idx = row * self.cols + col;
            if idx < c.len() {
                c[idx] = cell;
            }
        }
    }

    fn row_copy(&mut self, dst_row: usize, src_row: usize, n_cols: usize) {
        if let Some(ref mut c) = self.cells {
            let n = n_cols.min(self.cols);
            let src = src_row * self.cols;
            let dst = dst_row * self.cols;
            if src + n <= c.len() && dst + n <= c.len() {
                c.copy_within(src..src + n, dst);
            }
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
        if let Some(ref mut c) = self.cells {
            for cell in c.iter_mut() {
                *cell = Cell::blank();
            }
        }
    }

    /// Take ownership of a grid allocated by the caller.
    fn adopt(&mut self, cells: Option<KVec<Cell>>, cols: usize) {
        self.cells = cells;
        self.cols = cols;
    }
}

/// Grid dimensions for a `width` x `height` framebuffer at the given cell size.
/// Depends on nothing behind the console lock, so a caller can size and
/// allocate the grids before taking it.
fn grid_dims(width: u32, height: u32, cell_w: i32, cell_h: i32) -> (u16, u16) {
    let cols = (width / (cell_w.max(1) as u32)).max(1) as usize;
    let rows = (height / (cell_h.max(1) as u32)).max(1) as usize;
    (
        core::cmp::min(rows, VCONSOLE_MAX_ROWS) as u16,
        core::cmp::min(cols, VCONSOLE_MAX_COLS) as u16,
    )
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

fn color256_to_rgb(idx: u8) -> u32 {
    match idx {
        0..=7 => ANSI_COLORS[idx as usize],
        8..=15 => ANSI_BRIGHT_COLORS[(idx - 8) as usize],
        16..=231 => {
            // 6×6×6 color cube: index = 16 + 36*r + 6*g + b (each 0–5)
            let n = (idx - 16) as u32;
            let b = (n % 6) * 51;
            let g = ((n / 6) % 6) * 51;
            let r = (n / 36) * 51;
            (r << 16) | (g << 8) | b
        }
        232..=255 => {
            // Grayscale ramp: 24 shades from 8 to 238
            let v = (8 + 10 * (idx - 232) as u32) & 0xFF;
            (v << 16) | (v << 8) | v
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct VConsoleFbInfo {
    /// Kernel virtual address of the framebuffer's first byte. An integer
    /// rather than `*mut u8` so the type is `Send`/`Sync` without a marker;
    /// access goes through the `fb_blit` / `fb_put_pixel` helpers.
    pub(crate) base: u64,
    pub(crate) pitch: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bytes_per_pixel: u8,
}

/// Flat ring buffer of `SCROLLBACK_LINES` rows, heap-allocated once so
/// `VConsoleState` stores only a pointer.
struct ScrollbackBuf {
    buf: KVec<Cell>,
    cols: usize,
    head: usize,
    count: usize,
    view_offset: usize,
}

impl ScrollbackBuf {
    fn new(cols: usize) -> Self {
        let total = SCROLLBACK_LINES.saturating_mul(cols);
        let buf = if total > 0 {
            KVec::filled(Cell::blank(), total).unwrap_or_else(|_| KVec::new())
        } else {
            KVec::new()
        };
        Self {
            buf,
            cols,
            head: 0,
            count: 0,
            view_offset: 0,
        }
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
        self.head = (self.head + 1) % SCROLLBACK_LINES;
        if self.count < SCROLLBACK_LINES {
            self.count += 1;
        }
    }

    fn line_count(&self) -> usize {
        self.count
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

    fn viewing_history(&self) -> bool {
        self.view_offset > 0
    }

    #[cfg(feature = "test-hooks")]
    fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
        self.view_offset = 0;
    }
}

#[derive(slopos_ostd::SlotFields)]
pub(crate) struct VConsoleState {
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) cell_w: i32,
    pub(crate) cell_h: i32,
    pub(crate) fb: Option<VConsoleFbInfo>,
    pub(crate) cells: CellGrid,
    pub(crate) parser: VtParser,
    pub(crate) cursor_attrs: CursorAttributes,
    pub(crate) saved_cursor_row: u16,
    pub(crate) saved_cursor_col: u16,
    pub(crate) saved_cursor_attrs: CursorAttributes,
    pub(crate) cursor_visible: bool,
    pub(crate) alt_cells: CellGrid,
    pub(crate) alt_screen_cursor_row: u16,
    pub(crate) alt_screen_cursor_col: u16,
    pub(crate) in_alt_screen: bool,
    pub(crate) scroll_top: u16,
    pub(crate) scroll_bottom: u16,
    /// Shadow framebuffer — cached RAM, same dimensions as hardware FB.
    /// All glyph rendering targets this buffer; `flush_dirty()` blits to HW.
    shadow: Option<KVec<u8>>,
    shadow_pitch: usize,
    /// Bitmask of rows modified since last flush. Bit N = row N is dirty.
    dirty_rows: u128,
    /// A full-screen repaint is owed; run by [`run_pending_repaint`].
    repaint_pending: bool,
    /// Bumped whenever the grid geometry or the active screen changes; a
    /// banded repaint that observes a different epoch abandons itself.
    layout_epoch: u32,
}

/// Framebuffer ownership flag (Linux DRM/KMS master model).
///
/// While set, the vconsole keeps rendering to its shadow buffer but
/// `flush_dirty()` is a no-op, so no pixels reach the display.
static COMPOSITOR_OWNS_FB: AtomicBool = AtomicBool::new(false);

static SERIAL_MIRROR_ENABLED: AtomicBool = AtomicBool::new(true);

impl VConsoleState {
    pub(crate) const fn new() -> Self {
        Self {
            cursor_row: 0,
            cursor_col: 0,
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            cell_w: 8,
            cell_h: 16,
            fb: None,
            cells: CellGrid::empty(),
            parser: VtParser::new(),
            cursor_attrs: CursorAttributes::default_attrs(),
            saved_cursor_row: 0,
            saved_cursor_col: 0,
            saved_cursor_attrs: CursorAttributes::default_attrs(),
            cursor_visible: true,
            alt_cells: CellGrid::empty(),
            alt_screen_cursor_row: 0,
            alt_screen_cursor_col: 0,
            in_alt_screen: false,
            scroll_top: 0,
            scroll_bottom: 0,
            shadow: None,
            shadow_pitch: 0,
            dirty_rows: 0,
            repaint_pending: false,
            layout_epoch: 0,
        }
    }

    /// In-place [`Init`] recipe equivalent to [`Self::new`], so a runtime
    /// caller avoids the ~3 KiB stack frame `KBox::try_new` would produce.
    #[allow(dead_code)] // the static lock uses the const `Self::new`; this serves the test fixtures.
    pub(crate) fn init_default() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: slopos_ostd::mm::init::SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, cursor_row, 0);
                write_field!(slot, cursor_col, 0);
                write_field!(slot, rows, DEFAULT_ROWS);
                write_field!(slot, cols, DEFAULT_COLS);
                write_field!(slot, cell_w, 8);
                write_field!(slot, cell_h, 16);
                write_field!(slot, fb, None);
                write_field!(slot, cells, CellGrid::empty());
                write_field!(slot, parser, VtParser::new());
                write_field!(slot, cursor_attrs, CursorAttributes::default_attrs());
                write_field!(slot, saved_cursor_row, 0);
                write_field!(slot, saved_cursor_col, 0);
                write_field!(slot, saved_cursor_attrs, CursorAttributes::default_attrs());
                write_field!(slot, cursor_visible, true);
                write_field!(slot, alt_cells, CellGrid::empty());
                write_field!(slot, alt_screen_cursor_row, 0);
                write_field!(slot, alt_screen_cursor_col, 0);
                write_field!(slot, in_alt_screen, false);
                write_field!(slot, scroll_top, 0);
                write_field!(slot, scroll_bottom, 0);
                write_field!(slot, shadow, None);
                write_field!(slot, shadow_pitch, 0);
                write_field!(slot, dirty_rows, 0);
                write_field!(slot, repaint_pending, false);
                write_field!(slot, layout_epoch, 0);
                Ok(slot.finish())
            },
        )
    }

    pub(crate) fn process_byte(&mut self, b: u8) {
        let action = self.parser.advance(b);
        self.execute_action(action);
    }

    fn execute_action(&mut self, action: VtAction) {
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
    }

    fn print_codepoint(&mut self, cp: u32) {
        let row = self.cursor_row as usize;
        let col = self.cursor_col as usize;
        let rows = self.rows as usize;
        let cols = self.cols as usize;

        if row >= rows || col >= cols {
            return;
        }

        let wide = is_double_width(cp);
        let cell_attr = CellAttributes {
            fg: self.cursor_attrs.effective_fg(),
            bg: self.cursor_attrs.effective_bg(),
        };

        if wide && col + 1 < cols {
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
            self.render_cell(self.cursor_row, self.cursor_col);
            self.render_cell(self.cursor_row, self.cursor_col + 1);
            self.mark_row_dirty(self.cursor_row);
            self.cursor_col = self.cursor_col.saturating_add(2);
        } else if wide {
            self.cells.set(
                row,
                col,
                Cell {
                    codepoint: b' ' as u32,
                    attrs: cell_attr,
                },
            );
            self.render_cell(self.cursor_row, self.cursor_col);
            self.mark_row_dirty(self.cursor_row);
            self.cursor_col = self.cursor_col.saturating_add(1);
        } else {
            self.cells.set(
                row,
                col,
                Cell {
                    codepoint: cp,
                    attrs: cell_attr,
                },
            );
            self.render_cell(self.cursor_row, self.cursor_col);
            self.mark_row_dirty(self.cursor_row);
            self.cursor_col = self.cursor_col.saturating_add(1);
        }
        self.check_wrap_and_scroll();
    }

    fn execute_control(&mut self, ctrl: u8) {
        match ctrl {
            b'\n' | 0x0B | 0x0C => {
                self.cursor_row = self.cursor_row.saturating_add(1);
                self.cursor_col = 0;
            }
            b'\r' => {
                self.cursor_col = 0;
            }
            0x08 => {
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                    let row = self.cursor_row as usize;
                    let col = self.cursor_col as usize;
                    if row < self.rows as usize && col < self.cols as usize {
                        if self.cells.get(row, col).codepoint == CONTINUATION_CODEPOINT && col > 0 {
                            self.cells.set(row, col - 1, Cell::blank());
                            self.render_cell(self.cursor_row, (col - 1) as u16);
                        }
                        self.cells.set(row, col, Cell::blank());
                        self.render_cell(self.cursor_row, self.cursor_col);
                        self.mark_row_dirty(self.cursor_row);
                    }
                }
            }
            b'\t' => {
                let next = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next;
            }
            0x07 => {}
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
        let clear_cell = Cell {
            codepoint: b' ' as u32,
            attrs: CellAttributes {
                fg: self.cursor_attrs.effective_fg(),
                bg: self.cursor_attrs.effective_bg(),
            },
        };

        match mode {
            EraseMode::ToEnd => {
                if cr < rows {
                    for c in cc..cols {
                        self.cells.set(cr, c, clear_cell);
                    }
                    self.render_row_range(cr, cc, cols);
                    self.mark_row_dirty(cr as u16);
                }
                for r in (cr + 1)..rows {
                    self.clear_row_with_attr(r, clear_cell.attrs);
                }
                if cr + 1 < rows {
                    self.mark_rows_dirty((cr + 1) as u16, (rows - 1) as u16);
                }
            }
            EraseMode::ToStart => {
                for r in 0..cr {
                    self.clear_row_with_attr(r, clear_cell.attrs);
                }
                if cr > 0 {
                    self.mark_rows_dirty(0, (cr - 1) as u16);
                }
                if cr < rows {
                    let end = if cc < cols { cc + 1 } else { cols };
                    for c in 0..end {
                        self.cells.set(cr, c, clear_cell);
                    }
                    self.render_row_range(cr, 0, end);
                    self.mark_row_dirty(cr as u16);
                }
            }
            EraseMode::All => {
                for r in 0..rows {
                    self.clear_row_with_attr(r, clear_cell.attrs);
                }
                self.mark_all_dirty();
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
        let clear_cell = Cell {
            codepoint: b' ' as u32,
            attrs: CellAttributes {
                fg: self.cursor_attrs.effective_fg(),
                bg: self.cursor_attrs.effective_bg(),
            },
        };

        match mode {
            EraseMode::ToEnd => {
                for c in cc..cols {
                    self.cells.set(cr, c, clear_cell);
                }
                self.render_row_range(cr, cc, cols);
                self.mark_row_dirty(self.cursor_row);
            }
            EraseMode::ToStart => {
                let end = if cc < cols { cc + 1 } else { cols };
                for c in 0..end {
                    self.cells.set(cr, c, clear_cell);
                }
                self.render_row_range(cr, 0, end);
                self.mark_row_dirty(self.cursor_row);
            }
            EraseMode::All => {
                self.clear_row_with_attr(cr, clear_cell.attrs);
                self.mark_row_dirty(self.cursor_row);
            }
        }
    }

    fn scroll_up_n(&mut self, n: u16) {
        self.scroll_up_lines(n as usize);
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

        if let Some(ref mut shadow) = self.shadow {
            let row_px = self.cell_h as usize;
            let row_bytes = row_px.saturating_mul(self.shadow_pitch);
            let shift_bytes = shift.saturating_mul(row_bytes);
            let region_start = sr_top.saturating_mul(row_bytes);
            let region_end = (sr_bottom + 1).saturating_mul(row_bytes);
            if shift_bytes < region_end.saturating_sub(region_start) && row_bytes > 0 {
                shadow.copy_within(
                    region_start..region_end - shift_bytes,
                    region_start + shift_bytes,
                );
            }
            let clear_end = (region_start + shift_bytes).min(shadow.len());
            if region_start < clear_end {
                shadow[region_start..clear_end].fill(0);
            }
            self.mark_rows_dirty(sr_top as u16, sr_bottom as u16);
        } else {
            for r in sr_top..=sr_bottom {
                for c in 0..cols {
                    self.render_cell(r as u16, c as u16);
                }
            }
            self.mark_rows_dirty(sr_top as u16, sr_bottom as u16);
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
        self.alt_cells.copy_from(&self.cells, rows, cols);
        self.alt_screen_cursor_row = self.cursor_row;
        self.alt_screen_cursor_col = self.cursor_col;
        self.cells.clear_all();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.in_alt_screen = true;
        self.bump_layout_epoch();
        if let Some(ref mut shadow) = self.shadow {
            let row_bytes = (self.cell_h as usize).saturating_mul(self.shadow_pitch);
            let byte_count = rows.saturating_mul(row_bytes);
            let end = byte_count.min(shadow.len());
            shadow[..end].fill(0);
        } else {
            for r in 0..rows {
                for c in 0..cols {
                    self.render_cell_to_fb(r as u16, c as u16);
                }
            }
        }
        self.mark_all_dirty();
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
        self.bump_layout_epoch();
        self.request_repaint();
        self.mark_all_dirty();
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

        if let Some(ref mut shadow) = self.shadow {
            let row_px = self.cell_h as usize;
            let row_bytes = row_px.saturating_mul(self.shadow_pitch);
            let shift_bytes = shift.saturating_mul(row_bytes);
            let region_start = cur.saturating_mul(row_bytes);
            let region_end = (sr_bottom + 1).saturating_mul(row_bytes);
            if shift_bytes < region_end.saturating_sub(region_start) && row_bytes > 0 {
                shadow.copy_within(
                    region_start..region_end - shift_bytes,
                    region_start + shift_bytes,
                );
            }
            let clear_end = (region_start + shift_bytes).min(shadow.len());
            if region_start < clear_end {
                shadow[region_start..clear_end].fill(0);
            }
            self.mark_rows_dirty(sr_top as u16, sr_bottom as u16);
        } else {
            for r in sr_top..=sr_bottom {
                for c in 0..cols {
                    self.render_cell(r as u16, c as u16);
                }
            }
            self.mark_rows_dirty(sr_top as u16, sr_bottom as u16);
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
        for c in col..cols {
            self.render_cell(self.cursor_row, c as u16);
        }
        self.mark_row_dirty(self.cursor_row);
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
        for c in col..cols {
            self.render_cell(self.cursor_row, c as u16);
        }
        self.mark_row_dirty(self.cursor_row);
    }

    fn erase_chars(&mut self, n: u16) {
        let row = self.cursor_row as usize;
        let col = self.cursor_col as usize;
        let cols = self.cols as usize;
        if row >= self.rows as usize || col >= cols {
            return;
        }
        let end = (col + n as usize).min(cols);
        let clear_attr = CellAttributes {
            fg: self.cursor_attrs.effective_fg(),
            bg: self.cursor_attrs.effective_bg(),
        };
        let clear_cell = Cell {
            codepoint: b' ' as u32,
            attrs: clear_attr,
        };
        for c in col..end {
            self.cells.set(row, c, clear_cell);
            self.render_cell(self.cursor_row, c as u16);
        }
        self.mark_row_dirty(self.cursor_row);
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

        if let Some(ref mut shadow) = self.shadow {
            let row_px = self.cell_h as usize;
            let row_bytes = row_px.saturating_mul(self.shadow_pitch);
            let shift_bytes = shift.saturating_mul(row_bytes);
            let region_start = cur.saturating_mul(row_bytes);
            let region_end = (sr_bottom + 1).saturating_mul(row_bytes);
            if shift_bytes < region_end.saturating_sub(region_start) && row_bytes > 0 {
                shadow.copy_within(region_start + shift_bytes..region_end, region_start);
            }
            let clear_start = region_end.saturating_sub(shift_bytes);
            let clear_start = clear_start.min(shadow.len());
            let clear_end = region_end.min(shadow.len());
            if clear_start < clear_end {
                shadow[clear_start..clear_end].fill(0);
            }
            self.mark_rows_dirty(sr_top as u16, sr_bottom as u16);
        } else {
            for r in sr_top..=sr_bottom {
                for c in 0..cols {
                    self.render_cell(r as u16, c as u16);
                }
            }
            self.mark_rows_dirty(sr_top as u16, sr_bottom as u16);
        }
    }

    fn effective_scroll_bottom(&self) -> u16 {
        if self.scroll_bottom == 0 || self.scroll_bottom >= self.rows {
            self.rows.saturating_sub(1)
        } else {
            self.scroll_bottom
        }
    }

    #[inline(always)]
    fn mark_row_dirty(&mut self, row: u16) {
        if (row as u32) < 128 {
            self.dirty_rows |= 1u128 << row;
        }
    }

    #[inline(always)]
    fn mark_rows_dirty(&mut self, from: u16, to: u16) {
        for r in from..=to {
            self.mark_row_dirty(r);
        }
    }

    #[inline(always)]
    fn mark_all_dirty(&mut self) {
        if self.rows > 0 {
            self.mark_rows_dirty(0, self.rows.saturating_sub(1));
        }
    }

    fn check_wrap_and_scroll(&mut self) {
        if self.cursor_col >= self.cols {
            if self.parser.auto_wrap {
                self.cursor_col = 0;
                self.cursor_row = self.cursor_row.saturating_add(1);
            } else {
                self.cursor_col = self.cols.saturating_sub(1);
            }
        }
        let sr_bottom = self.effective_scroll_bottom();
        if self.cursor_row > sr_bottom {
            self.scroll_up_lines((self.cursor_row - sr_bottom) as usize);
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
        if !self.fill_shadow_row(row, attr.bg) {
            for c in 0..cols {
                self.render_cell(row as u16, c as u16);
            }
        }
        self.mark_row_dirty(row as u16);
    }

    /// Paint one text row's pixel band in `color`, returning whether it
    /// happened.
    ///
    /// The space glyph has no coverage, so writing the colour straight in is
    /// the same image without the per-pixel atlas lookup and blend.
    fn fill_shadow_row(&mut self, row: usize, color: u32) -> bool {
        let pitch = self.shadow_pitch;
        let width = (self.cols as usize).saturating_mul(self.cell_w as usize);
        let y0 = row.saturating_mul(self.cell_h as usize);
        let ch = self.cell_h as usize;
        let Some(ref mut shadow) = self.shadow else {
            return false;
        };
        if pitch == 0 || width == 0 {
            return false;
        }
        let bytes = color.to_ne_bytes();
        for y in y0..y0 + ch {
            let start = y.saturating_mul(pitch);
            let end = start.saturating_add(width.saturating_mul(4));
            if end > shadow.len() {
                break;
            }
            for pixel in shadow[start..end].chunks_exact_mut(4) {
                pixel.copy_from_slice(&bytes);
            }
        }
        true
    }

    fn render_row_range(&mut self, row: usize, col_start: usize, col_end: usize) {
        for c in col_start..col_end {
            self.render_cell(row as u16, c as u16);
        }
    }

    #[cfg_attr(
        not(feature = "test-hooks"),
        expect(dead_code, reason = "used by test-hooks feature")
    )]
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
                        self.cells.set(row, col, Cell::blank());
                        self.render_cell(self.cursor_row, self.cursor_col);
                        self.mark_row_dirty(self.cursor_row);
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
                    self.cells.set(
                        row,
                        col,
                        Cell {
                            codepoint: b as u32,
                            attrs: CellAttributes {
                                fg: self.cursor_attrs.effective_fg(),
                                bg: self.cursor_attrs.effective_bg(),
                            },
                        },
                    );
                    self.render_cell(self.cursor_row, self.cursor_col);
                    self.mark_row_dirty(self.cursor_row);
                    self.cursor_col = self.cursor_col.saturating_add(1);
                }
            }
            _ => {}
        }

        self.check_wrap_and_scroll();
    }

    /// Scroll the active region up by `n` lines in one shift: the shadow
    /// memmove spans the whole scroll region regardless of `n`, so N one-line
    /// scrolls cost N times what one N-line scroll does, interrupts off.
    pub(crate) fn scroll_up_lines(&mut self, n: usize) {
        let cols = self.cols as usize;
        let sr_top = self.scroll_top as usize;
        let sr_bottom = self.effective_scroll_bottom() as usize;
        if n == 0 || sr_bottom <= sr_top || cols == 0 {
            return;
        }
        let shift = n.min(sr_bottom - sr_top + 1);

        let full_screen = sr_top == 0 && sr_bottom == (self.rows as usize).saturating_sub(1);
        if full_screen && !self.in_alt_screen {
            if let Some(ref mut sb) = *SCROLLBACK.lock() {
                for row in sr_top..sr_top + shift {
                    sb.push_row(&self.cells, row, cols);
                }
                sb.reset_view();
            }
        }

        for row in (sr_top + shift)..=sr_bottom {
            self.cells.row_copy(row - shift, row, cols);
        }
        for row in (sr_bottom + 1 - shift)..=sr_bottom {
            for c in 0..cols {
                self.cells.set(row, c, Cell::blank());
            }
        }

        if let Some(ref mut shadow) = self.shadow {
            let row_px = self.cell_h as usize;
            let row_bytes = row_px.saturating_mul(self.shadow_pitch);
            let shift_bytes = shift.saturating_mul(row_bytes);
            let region_start = sr_top.saturating_mul(row_bytes);
            let region_end = (sr_bottom + 1).saturating_mul(row_bytes).min(shadow.len());
            if shift_bytes < region_end.saturating_sub(region_start) && row_bytes > 0 {
                shadow.copy_within(region_start + shift_bytes..region_end, region_start);
            }
            let clear_start = region_end.saturating_sub(shift_bytes);
            if clear_start < region_end {
                shadow[clear_start..region_end].fill(0);
            }
            self.mark_all_dirty();
        } else if self.fb.is_some() {
            for r in sr_top..=sr_bottom {
                for c in 0..cols {
                    self.render_cell(r as u16, c as u16);
                }
            }
        }
    }

    /// Render screen rows `[start, end)` from whatever is on screen: the
    /// scrollback view for the rows history occupies, the live grid below.
    /// The split is recomputed per band because the view can move while the
    /// console lock is released between bands.
    fn repaint_band(&mut self, start: u16, end: u16) {
        let cols = self.cols as usize;
        let end = end.min(self.rows);
        if start >= end || cols == 0 {
            return;
        }

        let guard = SCROLLBACK.lock();
        let sb_lines = match *guard {
            Some(ref sb) if sb.viewing_history() => sb.view_offset.min(self.rows as usize) as u16,
            _ => 0,
        };
        if let Some(ref sb) = *guard {
            for r in start..end.min(sb_lines) {
                let Some(sb_row) = sb.get_row(sb.view_offset - r as usize) else {
                    continue;
                };
                for c in 0..cols {
                    let cell = sb_row.get(c).copied().unwrap_or_else(Cell::blank);
                    self.render_cell_direct(r, c as u16, &cell);
                }
            }
        }
        drop(guard);

        for r in start.max(sb_lines)..end {
            for c in 0..cols {
                self.render_cell(r, c as u16);
            }
        }
        self.mark_rows_dirty(start, end - 1);
    }

    /// Ask for a full-screen repaint. Runs at the next
    /// [`run_pending_repaint`], outside the console lock.
    fn request_repaint(&mut self) {
        self.repaint_pending = true;
    }

    /// Invalidate any repaint already in flight over the old geometry.
    fn bump_layout_epoch(&mut self) {
        self.layout_epoch = self.layout_epoch.wrapping_add(1);
    }

    fn render_cell_direct(&mut self, row: u16, col: u16, cell: &Cell) {
        if self.shadow.is_some() {
            self.render_cell_direct_to_shadow(row, col, cell);
            return;
        }
        self.render_cell_direct_to_fb(row, col, cell);
    }

    fn render_cell_direct_to_fb(&self, row: u16, col: u16, cell: &Cell) {
        if COMPOSITOR_OWNS_FB.load(Ordering::Acquire) || slopos_ostd::fblog::is_active() {
            return;
        }
        let Some(fb) = self.fb else { return };
        let Some(atlas) = atlas::global() else {
            return;
        };
        let (r, c) = (row as usize, col as usize);
        if r >= self.rows as usize || c >= self.cols as usize {
            return;
        }
        if !self.atlas_matches_grid(&atlas) {
            return;
        }
        let cp = if cell.codepoint == CONTINUATION_CODEPOINT {
            b' ' as u32
        } else {
            cell.codepoint
        };
        let cw = self.cell_w as usize;
        let ch = self.cell_h as usize;
        let coverage = atlas.get_coverage(cp);
        let x0 = c * cw;
        let y0 = r * ch;
        for gy in 0..ch {
            for gx in 0..cw {
                let cov = coverage[gy * cw + gx];
                let color = blend_coverage_u32(cov, cell.attrs.fg, cell.attrs.bg);
                self.put_pixel(&fb, x0 + gx, y0 + gy, color);
            }
        }
    }

    /// Whether `atlas`'s cell geometry is the one the grid was laid out for.
    ///
    /// `atlas::replace_global` swaps the atlas without holding the console
    /// lock, so a glyph's coverage buffer can be sized for a cell this state
    /// has not adopted yet, and the row slices below would read past its end.
    #[inline]
    fn atlas_matches_grid(&self, atlas: &atlas::AtlasGuard) -> bool {
        atlas.cell_width() == self.cell_w && atlas.cell_height() == self.cell_h
    }

    fn render_cell_direct_to_shadow(&mut self, row: u16, col: u16, cell: &Cell) {
        let (r, c) = (row as usize, col as usize);
        if r >= self.rows as usize || c >= self.cols as usize {
            return;
        }
        let Some(ref mut shadow) = self.shadow else {
            self.render_cell_direct_to_fb(row, col, cell);
            return;
        };
        let Some(atlas) = atlas::global() else {
            return;
        };
        if atlas.cell_width() != self.cell_w || atlas.cell_height() != self.cell_h {
            return;
        }
        let cp = if cell.codepoint == CONTINUATION_CODEPOINT {
            b' ' as u32
        } else {
            cell.codepoint
        };
        let cw = self.cell_w as usize;
        let ch = self.cell_h as usize;
        let coverage = atlas.get_coverage(cp);
        let x0 = c.saturating_mul(cw);
        let y0 = r.saturating_mul(ch);
        let row_bytes = cw.saturating_mul(4);
        for gy in 0..ch {
            let row_offset = (y0 + gy)
                .saturating_mul(self.shadow_pitch)
                .saturating_add(x0.saturating_mul(4));
            if row_offset + row_bytes <= shadow.len() {
                Self::expand_coverage_row(
                    &mut shadow[row_offset..row_offset + row_bytes],
                    &coverage[gy * cw..(gy + 1) * cw],
                    cell.attrs.fg,
                    cell.attrs.bg,
                );
            }
        }
    }

    #[inline(always)]
    fn expand_coverage_row(dst: &mut [u8], coverage: &[u8], fg: u32, bg: u32) {
        for (gx, &cov) in coverage.iter().enumerate() {
            let pixel = blend_coverage_u32(cov, fg, bg);
            let off = gx * 4;
            if off + 4 <= dst.len() {
                let bytes = pixel.to_ne_bytes();
                dst[off] = bytes[0];
                dst[off + 1] = bytes[1];
                dst[off + 2] = bytes[2];
                dst[off + 3] = bytes[3];
            }
        }
    }

    pub(crate) fn render_cell_to_fb(&self, row: u16, col: u16) {
        if COMPOSITOR_OWNS_FB.load(Ordering::Acquire) || slopos_ostd::fblog::is_active() {
            return;
        }
        let Some(fb) = self.fb else {
            return;
        };
        let Some(atlas) = atlas::global() else {
            return;
        };

        let row_usize = row as usize;
        let col_usize = col as usize;
        if row_usize >= self.rows as usize || col_usize >= self.cols as usize {
            return;
        }
        if !self.atlas_matches_grid(&atlas) {
            return;
        }

        let cell = self.cells.get(row_usize, col_usize);
        let cp = if cell.codepoint == CONTINUATION_CODEPOINT {
            b' ' as u32
        } else {
            cell.codepoint
        };
        let cw = self.cell_w as usize;
        let ch = self.cell_h as usize;
        let coverage = atlas.get_coverage(cp);
        let x0 = col_usize.saturating_mul(cw);
        let y0 = row_usize.saturating_mul(ch);

        for gy in 0..ch {
            for gx in 0..cw {
                let cov = coverage[gy * cw + gx];
                let color = blend_coverage_u32(cov, cell.attrs.fg, cell.attrs.bg);
                self.put_pixel(&fb, x0 + gx, y0 + gy, color);
            }
        }
    }

    pub(crate) fn render_cell(&mut self, row: u16, col: u16) {
        if self.shadow.is_some() {
            let row_usize = row as usize;
            let col_usize = col as usize;
            if row_usize >= self.rows as usize || col_usize >= self.cols as usize {
                return;
            }
            let cell = self.cells.get(row_usize, col_usize);
            self.render_cell_direct_to_shadow(row, col, &cell);
            return;
        }
        self.render_cell_to_fb(row, col);
    }

    fn flush_dirty(&mut self) {
        if self.dirty_rows == 0 {
            return;
        }
        // Compositor owns the framebuffer: keep the dirty bits for
        // `compositor_release_fb()` rather than clearing them.
        if COMPOSITOR_OWNS_FB.load(Ordering::Acquire) || slopos_ostd::fblog::is_active() {
            return;
        }
        let dirty = self.dirty_rows;
        let Some(fb) = self.fb else {
            self.dirty_rows = 0;
            return;
        };
        let Some(ref shadow) = self.shadow else {
            self.dirty_rows = 0;
            return;
        };

        let row_height = self.cell_h as usize;
        let pitch = fb.pitch as usize;
        let max_rows = self.rows as usize;
        let mut bits = dirty;

        while bits != 0 {
            let start = bits.trailing_zeros() as usize;
            if start >= max_rows {
                break;
            }
            bits &= !(1u128 << start);
            let mut end = start;
            while end + 1 < max_rows && (bits & (1u128 << (end + 1))) != 0 {
                end += 1;
                bits &= !(1u128 << end);
            }

            let y_start = start.saturating_mul(row_height);
            let y_end = (end + 1).saturating_mul(row_height);
            let byte_offset = y_start.saturating_mul(pitch);
            let byte_count = y_end.saturating_sub(y_start).saturating_mul(pitch);

            if byte_offset + byte_count <= shadow.len() {
                fb_blit(
                    fb.base,
                    byte_offset,
                    &shadow[byte_offset..byte_offset + byte_count],
                );
            }
        }

        self.dirty_rows = 0;
        #[cfg(target_arch = "x86_64")]
        slopos_ostd::arch::x86_64::mem_fence::sfence();
    }

    /// Common tail of every geometry change: invalidate in-flight repaints,
    /// reset the scroll region and clamp the cursor into the new grid.
    fn settle_geometry(&mut self) {
        self.bump_layout_epoch();
        self.mark_all_dirty();

        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);

        if self.cursor_col >= self.cols {
            self.cursor_col = self.cols.saturating_sub(1);
        }
        if self.cursor_row >= self.rows {
            self.cursor_row = self.rows.saturating_sub(1);
        }
    }

    #[inline(always)]
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

        fb_put_pixel(fb.base, offset, fb.bytes_per_pixel, color);
    }
}

/// Copy `src.len()` bytes into the framebuffer mapping at `base + byte_offset`.
/// Caller must ensure `byte_offset + src.len()` is inside the framebuffer.
#[inline]
fn fb_blit(base: u64, byte_offset: usize, src: &[u8]) {
    slopos_ostd::boot::handoff::framebuffer::fb_blit_bytes(base, byte_offset, src);
}

/// Write a single pixel at `base + offset` using the given pixel format.
/// `offset` is bounds-checked by the caller (`put_pixel`'s width/height gate).
#[inline]
fn fb_put_pixel(base: u64, offset: usize, bytes_per_pixel: u8, color: u32) {
    use slopos_ostd::boot::handoff::framebuffer::{
        fb_ptr_add, fb_write_u8_at, fb_write_u16_at, fb_write_u32_unaligned,
    };
    let p = fb_ptr_add(base as *mut u8, offset);
    match bytes_per_pixel {
        4 => {
            let bgra = color & 0x00FF_FFFF;
            fb_write_u32_unaligned(p, bgra);
        }
        3 => {
            fb_write_u8_at(p, (color & 0xFF) as u8);
            fb_write_u8_at(fb_ptr_add(p, 1), ((color >> 8) & 0xFF) as u8);
            fb_write_u8_at(fb_ptr_add(p, 2), ((color >> 16) & 0xFF) as u8);
        }
        2 => {
            let r = ((color >> 16) & 0xFF) as u16;
            let g = ((color >> 8) & 0xFF) as u16;
            let b = (color & 0xFF) as u16;
            let rgb565 = ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3);
            fb_write_u16_at(p, rgb565);
        }
        _ => {}
    }
}

static VCONSOLE_STATE: SpinLock<VConsoleState> = SpinLock::new(
    VConsoleState::new(),
    lock_class!("VCONSOLE_STATE", LOCK_LEVEL_RESOURCE),
);
static SCROLLBACK: SpinLock<Option<KBox<ScrollbackBuf>>> =
    SpinLock::new(None, lock_class!("SCROLLBACK", LOCK_LEVEL_RESOURCE));

/// Screen rows repainted per console-lock hold.
const REPAINT_BAND_ROWS: u16 = 4;

/// Run an owed full-screen repaint, releasing the console lock between row
/// bands.
///
/// Callers reaching the console through the TTY layer must invoke this with
/// `TTY_WRITE_LOCKS` released: a repaint emits no bytes, and holding it here
/// would mask interrupts for the whole screen however finely the console lock
/// is banded. `VCONSOLE_STATE` is IRQ-disabling and a full-screen glyph
/// rasterisation is 8.3 M pixels at 4K, so banding bounds one hold to a few
/// rows' worth of work.
///
/// Concurrent output is not lost: a row written before the band reaches it is
/// rendered with its new content, one written after paints and marks itself
/// dirty. A repaint that outlives the geometry it started on abandons itself.
pub fn run_pending_repaint() {
    let epoch = {
        let mut state = VCONSOLE_STATE.lock();
        if !core::mem::take(&mut state.repaint_pending) {
            return;
        }
        state.layout_epoch
    };

    let mut row: u16 = 0;
    loop {
        let mut state = VCONSOLE_STATE.lock();
        if state.layout_epoch != epoch || row >= state.rows {
            return;
        }
        let end = row.saturating_add(REPAINT_BAND_ROWS).min(state.rows);
        state.repaint_band(row, end);
        state.flush_dirty();
        row = end;
    }
}

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

    atlas::register_font_change_callback(notify_font_changed);

    // Every allocation happens before the SpinLock is taken: allocating with
    // interrupts disabled trips UB checks in the global allocator, and the
    // buddy allocator's reuse path can wait on a cross-CPU TLB drain.
    let (cell_w, cell_h) = atlas::global().map_or((8, 16), |a| (a.cell_width(), a.cell_height()));
    let (rows, cols) = grid_dims(width, height, cell_w, cell_h);
    let grid_len = (rows as usize).saturating_mul(cols as usize);
    let cells = KVec::filled(Cell::blank(), grid_len).ok();
    let alt_cells = KVec::filled(Cell::blank(), grid_len).ok();
    let scrollback =
        KBox::try_new(ScrollbackBuf::new(cols as usize)).expect("vconsole: scrollback alloc");
    let shadow_size = (pitch as usize).saturating_mul(height as usize);
    let shadow = if shadow_size > 0 {
        KVec::<u8>::zeroed(shadow_size).ok()
    } else {
        None
    };

    {
        let mut state = VCONSOLE_STATE.lock();
        state.fb = Some(VConsoleFbInfo {
            base: base as u64,
            pitch,
            width,
            height,
            bytes_per_pixel,
        });
        state.cell_w = cell_w;
        state.cell_h = cell_h;
        state.rows = rows;
        state.cols = cols;
        state.cells.adopt(cells, cols as usize);
        state.alt_cells.adopt(alt_cells, cols as usize);
        state.settle_geometry();
        // Don't clear_row() — would paint over the active splash screen.
        state.cursor_row = 0;
        state.cursor_col = 0;
        state.shadow_pitch = if shadow.is_some() { pitch as usize } else { 0 };
        state.shadow = shadow;
    }
    *SCROLLBACK.lock() = Some(scrollback);
}

pub fn write(data: &[u8]) {
    let was_viewing_history = {
        let sb_guard = SCROLLBACK.lock();
        sb_guard.as_ref().is_some_and(|sb| sb.viewing_history())
    };

    if was_viewing_history {
        if let Some(ref mut sb) = *SCROLLBACK.lock() {
            sb.reset_view();
        }
    }

    let mut state = VCONSOLE_STATE.lock();
    if state.fb.is_none() {
        // No framebuffer (headless boot or -nographic), and deliberately no
        // serial fallback: `tty::driver::write_driver_unlocked` already mirrors
        // to COM1 under the klog ticket lock, so emitting here would duplicate
        // every TTY write and do it lock-free, corrupting concurrent klog.
        return;
    }

    if was_viewing_history {
        state.request_repaint();
    }

    for &b in data {
        state.process_byte(b);
    }

    state.flush_dirty();
}

pub fn has_framebuffer() -> bool {
    VCONSOLE_STATE.lock().fb.is_some()
}

pub fn notify_font_changed() {
    let (atlas_gen, need_resize, new_cw, new_ch, new_rows, new_cols, fb_info, old_rows, old_cols) = {
        let state = VCONSOLE_STATE.lock();
        let Some(fb) = state.fb else {
            return;
        };
        let Some(atlas) = atlas::global() else {
            return;
        };
        let atlas_gen = atlas::atlas_generation();
        let cw = atlas.cell_width();
        let ch = atlas.cell_height();
        if cw == state.cell_w && ch == state.cell_h {
            return;
        }
        let calc_cols = (fb.width / cw as u32).max(1) as usize;
        let calc_rows = (fb.height / ch as u32).max(1) as usize;
        let nc = calc_cols.min(VCONSOLE_MAX_COLS);
        let nr = calc_rows.min(VCONSOLE_MAX_ROWS);
        (
            atlas_gen,
            true,
            cw,
            ch,
            nr,
            nc,
            fb,
            state.rows as usize,
            state.cols as usize,
        )
    };

    if !need_resize {
        return;
    }

    let shadow_size = (fb_info.pitch as usize).saturating_mul(fb_info.height as usize);
    let new_shadow = if shadow_size > 0 {
        KVec::<u8>::zeroed(shadow_size).ok()
    } else {
        None
    };
    let new_scrollback: Option<KBox<ScrollbackBuf>> =
        KBox::try_new(ScrollbackBuf::new(new_cols)).ok();
    let new_grid = {
        let grid_len = new_rows * new_cols;
        KVec::filled(Cell::blank(), grid_len).ok()
    };
    let new_alt_grid = {
        let grid_len = new_rows * new_cols;
        KVec::filled(Cell::blank(), grid_len).ok()
    };

    if new_scrollback.is_none() || new_grid.is_none() || new_alt_grid.is_none() {
        return;
    }
    let new_scrollback = new_scrollback.unwrap();
    let new_grid = new_grid.unwrap();
    let new_alt_grid = new_alt_grid.unwrap();

    let mut state = VCONSOLE_STATE.lock();
    if state.fb.is_none() {
        return;
    }
    if (atlas::atlas_generation()) != atlas_gen {
        return;
    }

    let old_cells = core::mem::replace(&mut state.cells, CellGrid::empty());
    state.cell_w = new_cw;
    state.cell_h = new_ch;
    state.rows = new_rows as u16;
    state.cols = new_cols as u16;
    state.cells = CellGrid {
        cells: Some(new_grid),
        cols: new_cols,
    };
    state.alt_cells = CellGrid {
        cells: Some(new_alt_grid),
        cols: new_cols,
    };
    let copy_rows = old_rows.min(new_rows);
    let copy_cols = old_cols.min(new_cols);
    state.cells.copy_from(&old_cells, copy_rows, copy_cols);
    state.scroll_top = 0;
    state.scroll_bottom = state.rows.saturating_sub(1);
    if state.cursor_col >= state.cols {
        state.cursor_col = state.cols.saturating_sub(1);
    }
    if state.cursor_row >= state.rows {
        state.cursor_row = state.rows.saturating_sub(1);
    }
    state.shadow = new_shadow;
    state.shadow_pitch = if state.shadow.is_some() {
        fb_info.pitch as usize
    } else {
        0
    };
    state.bump_layout_epoch();
    state.request_repaint();
    drop(state);

    *SCROLLBACK.lock() = Some(new_scrollback);

    run_pending_repaint();
}

pub fn scroll_view_up(lines: usize) {
    if let Some(ref mut sb) = *SCROLLBACK.lock() {
        sb.scroll_up(lines);
    }
    VCONSOLE_STATE.lock().request_repaint();
    run_pending_repaint();
}

pub fn scroll_view_down(lines: usize) {
    if let Some(ref mut sb) = *SCROLLBACK.lock() {
        sb.scroll_down(lines);
    }
    VCONSOLE_STATE.lock().request_repaint();
    run_pending_repaint();
}

/// Transfer framebuffer ownership to the compositor (Linux `DRM_IOCTL_SET_MASTER`
/// equivalent). The vconsole keeps rendering to its shadow buffer, preserving
/// terminal state, but stops copying to the hardware framebuffer.
pub fn compositor_acquire_fb() {
    COMPOSITOR_OWNS_FB.store(true, Ordering::Release);
}

/// Return framebuffer ownership to the vconsole (compositor crash recovery).
///
/// Re-enables `flush_dirty()` and immediately blits the shadow buffer to
/// restore the kernel console display, matching Linux's fbcon re-bind on
/// DRM master drop.
pub fn compositor_release_fb() {
    COMPOSITOR_OWNS_FB.store(false, Ordering::Release);
    // Full shadow → FB blit to restore the console display.
    let mut state = VCONSOLE_STATE.lock();
    state.mark_all_dirty();
    state.flush_dirty();
}

pub fn set_serial_mirror(enabled: bool) {
    SERIAL_MIRROR_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn serial_mirror_enabled() -> bool {
    SERIAL_MIRROR_ENABLED.load(Ordering::Relaxed)
}

pub fn scrollback_line_count() -> usize {
    SCROLLBACK.lock().as_ref().map_or(0, |sb| sb.line_count())
}

#[cfg(feature = "test-hooks")]
pub(crate) fn reset_for_tests() {
    let mut state = VCONSOLE_STATE.lock();
    state.cursor_row = 0;
    state.cursor_col = 0;
    state.rows = DEFAULT_ROWS;
    state.cols = DEFAULT_COLS;
    state.cell_w = 8;
    state.cell_h = 16;
    state.fb = None;
    state
        .cells
        .allocate(DEFAULT_ROWS as usize, DEFAULT_COLS as usize);
    state.parser = VtParser::new();
    state.cursor_attrs = CursorAttributes::default_attrs();
    state.saved_cursor_row = 0;
    state.saved_cursor_col = 0;
    state.saved_cursor_attrs = CursorAttributes::default_attrs();
    state.cursor_visible = true;
    state
        .alt_cells
        .allocate(DEFAULT_ROWS as usize, DEFAULT_COLS as usize);
    state.alt_screen_cursor_row = 0;
    state.alt_screen_cursor_col = 0;
    state.in_alt_screen = false;
    state.scroll_top = 0;
    state.scroll_bottom = DEFAULT_ROWS.saturating_sub(1);
    state.shadow = None;
    state.shadow_pitch = 0;
    state.dirty_rows = 0;
    if let Some(ref mut sb) = *SCROLLBACK.lock() {
        sb.clear();
    }
}
