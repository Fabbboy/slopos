//! VConsole - framebuffer-backed virtual console text renderer.
//!
//! Manages cursor position, cell buffer, and direct framebuffer rendering
//! for TTY 1 (the virtual console). When no framebuffer is registered
//! (early boot or headless), output falls back to serial mirroring.

use core::ptr;

use slopos_abi::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, get_glyph_or_space};
use slopos_lib::IrqMutex;

use crate::serial::serial_putc_com1;

pub(crate) const VCONSOLE_MAX_COLS: usize = 240;
pub(crate) const VCONSOLE_MAX_ROWS: usize = 80;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 25;
const FG_COLOR: u32 = 0x00AAAAAA;
const BG_COLOR: u32 = 0x00000000;

#[derive(Clone, Copy)]
pub(crate) struct VConsoleFbInfo {
    pub(crate) base: *mut u8,
    pub(crate) pitch: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bytes_per_pixel: u8,
}

unsafe impl Send for VConsoleFbInfo {}

pub(crate) struct VConsoleState {
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) fb: Option<VConsoleFbInfo>,
    pub(crate) cells: [[u8; VCONSOLE_MAX_COLS]; VCONSOLE_MAX_ROWS],
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
        }
    }

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
                    self.render_cell(self.cursor_row, self.cursor_col);
                    self.cursor_col = self.cursor_col.saturating_add(1);
                }
            }
            _ => {}
        }

        if self.cursor_col >= self.cols {
            self.cursor_col = 0;
            self.cursor_row = self.cursor_row.saturating_add(1);
        }
        if self.cursor_row >= self.rows {
            self.scroll_up();
            self.cursor_row = self.rows.saturating_sub(1);
        }
    }

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

        let glyph = get_glyph_or_space(self.cells[row_usize][col_usize]);
        let x0 = col_usize.saturating_mul(FONT_CHAR_WIDTH as usize);
        let y0 = row_usize.saturating_mul(FONT_CHAR_HEIGHT as usize);

        for gy in 0..FONT_CHAR_HEIGHT as usize {
            let bits = glyph[gy];
            for gx in 0..FONT_CHAR_WIDTH as usize {
                let mask = 1u8 << (7 - gx as u8);
                let color = if (bits & mask) != 0 {
                    FG_COLOR
                } else {
                    BG_COLOR
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
        state.write_byte(b);
    }
}

pub fn has_framebuffer() -> bool {
    VCONSOLE_STATE.lock().fb.is_some()
}

#[cfg(feature = "itests")]
pub(crate) fn reset_for_tests() {
    *VCONSOLE_STATE.lock() = VConsoleState::new();
}
