//! Shell implementation with Cell-based state management
//!
//! Uses Cell<T> for Copy types to avoid RefCell borrow conflicts.
//! Drawing functions take explicit &mut DrawBuffer parameters.
//!
//! # Safety
//! This module uses static variables with interior mutability. This is safe
//! because userland is single-threaded with no preemption during shell code.

use core::cell::{Cell, UnsafeCell}; // UnsafeCell used by SyncUnsafeCell wrapper
use core::cmp;
use core::ffi::{c_char, c_void};
use core::ptr;

// =============================================================================
// Sync wrappers for single-threaded userland
// =============================================================================

/// UnsafeCell wrapper that implements Sync for single-threaded userland
#[repr(transparent)]
struct SyncUnsafeCell<T>(UnsafeCell<T>);

impl<T> SyncUnsafeCell<T> {
    const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    #[inline]
    fn get(&self) -> *mut T {
        self.0.get()
    }
}

// Safety: Userland is single-threaded with no preemption during shell code
unsafe impl<T> Sync for SyncUnsafeCell<T> {}

use crate::gfx::{self, DrawBuffer};
use crate::runtime;
use crate::syscall::{
    ShmBuffer, USER_FS_OPEN_CREAT, USER_FS_OPEN_READ, USER_FS_OPEN_WRITE, UserFbInfo, UserFsEntry,
    UserFsList, UserSysInfo, sys_fb_info, sys_fs_close, sys_fs_list, sys_fs_mkdir, sys_fs_open,
    sys_fs_read, sys_fs_unlink, sys_fs_write, sys_halt, sys_read_char, sys_surface_commit,
    sys_surface_set_title, sys_sys_info, sys_write,
};

const SHELL_MAX_TOKENS: usize = 16;
const SHELL_MAX_TOKEN_LENGTH: usize = 64;
const SHELL_PATH_BUF: usize = 128;
const SHELL_IO_MAX: usize = 512;

#[unsafe(link_section = ".user_rodata")]
static PROMPT: &[u8] = b"$ ";
#[unsafe(link_section = ".user_rodata")]
static NL: &[u8] = b"\n";
#[unsafe(link_section = ".user_rodata")]
static WELCOME: &[u8] = b"SlopOS Shell v0.1 (userland)\n";
#[unsafe(link_section = ".user_rodata")]
static HELP_HEADER: &[u8] = b"Available commands:\n";
#[unsafe(link_section = ".user_rodata")]
static UNKNOWN_CMD: &[u8] = b"Unknown command. Type 'help'.\n";
#[unsafe(link_section = ".user_rodata")]
static PATH_TOO_LONG: &[u8] = b"path too long\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_NO_SUCH: &[u8] = b"No such file or directory\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_TOO_MANY_ARGS: &[u8] = b"too many arguments\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_MISSING_OPERAND: &[u8] = b"missing operand\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_MISSING_FILE: &[u8] = b"missing file operand\n";
#[unsafe(link_section = ".user_rodata")]
static ERR_MISSING_TEXT: &[u8] = b"missing text operand\n";
#[unsafe(link_section = ".user_rodata")]
static HALTED: &[u8] = b"Shell requested shutdown...\n";

const FONT_CHAR_WIDTH: i32 = 8;
const FONT_CHAR_HEIGHT: i32 = 16;
const SHELL_BG_COLOR: u32 = 0x1E1E_1EFF;
const SHELL_FG_COLOR: u32 = 0xE6E6_E6FF;

const SHELL_WINDOW_WIDTH: i32 = 640;
const SHELL_WINDOW_HEIGHT: i32 = 480;
const SHELL_TAB_WIDTH: i32 = 4;
const SHELL_SCROLLBACK_LINES: usize = 256;
const SHELL_SCROLLBACK_COLS: usize = 160;
const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;

// =============================================================================
// DisplayState: Cell-based state (no borrow conflicts)
// =============================================================================

struct DisplayState {
    enabled: Cell<bool>,
    width: Cell<i32>,
    height: Cell<i32>,
    pitch: Cell<usize>,
    bytes_pp: Cell<u8>,
    cols: Cell<i32>,
    rows: Cell<i32>,
    cursor_col: Cell<i32>,
    cursor_line: Cell<i32>,
    origin: Cell<i32>,
    total_lines: Cell<i32>,
    view_top: Cell<i32>,
    follow: Cell<bool>,
    fg: Cell<u32>,
    bg: Cell<u32>,
}

impl DisplayState {
    const fn new() -> Self {
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

    fn line_slot(&self, logical: i32) -> usize {
        let max_lines = SHELL_SCROLLBACK_LINES as i32;
        ((self.origin.get() + logical).rem_euclid(max_lines)) as usize
    }

    fn cursor(&self) -> (i32, i32) {
        let row = (self.cursor_line.get() - self.view_top.get())
            .clamp(0, self.rows.get().saturating_sub(1));
        (self.cursor_col.get(), row)
    }

    fn reset(&self) {
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

#[unsafe(link_section = ".user_bss")]
static DISPLAY: DisplayState = DisplayState::new();

// =============================================================================
// Scrollback module: safe accessors for large arrays
// =============================================================================

mod scrollback {
    use super::*;

    #[unsafe(link_section = ".user_bss")]
    static DATA: SyncUnsafeCell<[u8; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS]> =
        SyncUnsafeCell::new([0; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS]);

    #[unsafe(link_section = ".user_bss")]
    static LENS: SyncUnsafeCell<[u16; SHELL_SCROLLBACK_LINES]> =
        SyncUnsafeCell::new([0; SHELL_SCROLLBACK_LINES]);

    /// Safety: Single-threaded userland, no preemption during shell code
    #[inline]
    pub fn get_line(slot: usize) -> &'static [u8] {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe {
            let data = &*DATA.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            &data[start..start + SHELL_SCROLLBACK_COLS]
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

    pub fn clear_line(slot: usize) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe {
            let data = &mut *DATA.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            for i in start..start + SHELL_SCROLLBACK_COLS {
                data[i] = 0;
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
            let lens = &mut *LENS.get();
            for len in lens.iter_mut() {
                *len = 0;
            }
        }
    }

    /// Write a line to scrollback (for prompt rewriting)
    pub fn write_line(slot: usize, content: &[u8]) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let len = content.len().min(SHELL_SCROLLBACK_COLS);
        unsafe {
            let data = &mut *DATA.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            // Clear first
            for i in start..start + SHELL_SCROLLBACK_COLS {
                data[i] = 0;
            }
            // Write content
            for (i, &b) in content.iter().take(len).enumerate() {
                data[start + i] = b;
            }
            (*LENS.get())[slot] = len as u16;
        }
    }
}

// =============================================================================
// Surface module: drawing buffer management
// =============================================================================

mod surface {
    use super::*;

    struct ShellSurface {
        shm_buffer: Option<ShmBuffer>,
        width: i32,
        height: i32,
        pitch: usize,
        bytes_pp: u8,
    }

    impl ShellSurface {
        const fn empty() -> Self {
            Self {
                shm_buffer: None,
                width: 0,
                height: 0,
                pitch: 0,
                bytes_pp: 4,
            }
        }

        fn draw_buffer(&mut self) -> Option<DrawBuffer<'_>> {
            let buf = self.shm_buffer.as_mut()?;
            DrawBuffer::new(
                buf.as_mut_slice(),
                self.width as u32,
                self.height as u32,
                self.pitch,
                self.bytes_pp,
            )
        }
    }

    #[unsafe(link_section = ".user_bss")]
    static SURFACE: SyncUnsafeCell<ShellSurface> = SyncUnsafeCell::new(ShellSurface::empty());

    /// Access surface for drawing. Safety: single-threaded userland
    fn with_surface<R, F: FnOnce(&mut ShellSurface) -> R>(f: F) -> R {
        f(unsafe { &mut *SURFACE.get() })
    }

    pub fn init(width: i32, height: i32, bpp: u8) -> bool {
        with_surface(|s| {
            s.width = width;
            s.height = height;
            s.bytes_pp = ((bpp as usize + 7) / 8) as u8;
            s.pitch = (width as usize) * (s.bytes_pp as usize);

            let buffer_size = s.pitch * (height as usize);
            let shm_buffer = match ShmBuffer::create(buffer_size) {
                Ok(buf) => buf,
                Err(_) => {
                    let _ = sys_write(b"shell: failed to create shm buffer\n");
                    return false;
                }
            };

            if shm_buffer
                .attach_surface(width as u32, height as u32)
                .is_err()
            {
                let _ = sys_write(b"shell: failed to attach surface\n");
                return false;
            }

            s.shm_buffer = Some(shm_buffer);
            true
        })
    }

    pub fn draw<R, F: FnOnce(&mut DrawBuffer) -> R>(f: F) -> Option<R> {
        with_surface(|s| s.draw_buffer().map(|mut buf| f(&mut buf)))
    }
}

// =============================================================================
// Command buffers module: safe accessors
// =============================================================================

mod buffers {
    use super::*;

    #[unsafe(link_section = ".user_bss")]
    static LINE_BUF: SyncUnsafeCell<[u8; 256]> = SyncUnsafeCell::new([0; 256]);

    #[unsafe(link_section = ".user_bss")]
    static TOKEN_STORAGE: SyncUnsafeCell<[[u8; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS]> =
        SyncUnsafeCell::new([[0; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS]);

    #[unsafe(link_section = ".user_bss")]
    static PATH_BUF: SyncUnsafeCell<[u8; SHELL_PATH_BUF]> =
        SyncUnsafeCell::new([0; SHELL_PATH_BUF]);

    #[unsafe(link_section = ".user_bss")]
    static LIST_ENTRIES: SyncUnsafeCell<[UserFsEntry; 32]> =
        SyncUnsafeCell::new([UserFsEntry::new(); 32]);

    pub fn with_line_buf<R, F: FnOnce(&mut [u8; 256]) -> R>(f: F) -> R {
        f(unsafe { &mut *LINE_BUF.get() })
    }

    pub fn with_token_storage<
        R,
        F: FnOnce(&mut [[u8; SHELL_MAX_TOKEN_LENGTH]; SHELL_MAX_TOKENS]) -> R,
    >(
        f: F,
    ) -> R {
        f(unsafe { &mut *TOKEN_STORAGE.get() })
    }

    pub fn with_path_buf<R, F: FnOnce(&mut [u8; SHELL_PATH_BUF]) -> R>(f: F) -> R {
        f(unsafe { &mut *PATH_BUF.get() })
    }

    pub fn with_list_entries<R, F: FnOnce(&mut [UserFsEntry; 32]) -> R>(f: F) -> R {
        f(unsafe { &mut *LIST_ENTRIES.get() })
    }

    pub fn token_ptr(idx: usize) -> *const u8 {
        unsafe { (*TOKEN_STORAGE.get())[idx].as_ptr() }
    }
}

// =============================================================================
// Free drawing functions (no &mut self, explicit parameters)
// =============================================================================

fn draw_char_at(buf: &mut DrawBuffer, col: i32, row: i32, c: u8, fg: u32, bg: u32) {
    let x = col * FONT_CHAR_WIDTH;
    let y = row * FONT_CHAR_HEIGHT;
    gfx::font::draw_char(buf, x, y, c, fg, bg);
}

fn clear_row(buf: &mut DrawBuffer, row: i32, width: i32, bg: u32) {
    gfx::fill_rect(buf, 0, row * FONT_CHAR_HEIGHT, width, FONT_CHAR_HEIGHT, bg);
}

fn draw_row_from_scrollback(buf: &mut DrawBuffer, display: &DisplayState, logical: i32, row: i32) {
    let bg = display.bg.get();
    let fg = display.fg.get();
    let width = display.width.get();
    let cols = display.cols.get();
    let total_lines = display.total_lines.get();

    // Clear the row first
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

    let line = scrollback::get_line(slot);
    for (col, &ch) in line.iter().take(draw_len).enumerate() {
        if ch != 0 {
            draw_char_at(buf, col as i32, row, ch, fg, bg);
        }
    }
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

    if height <= FONT_CHAR_HEIGHT {
        return false;
    }

    // Blit content up by one line
    gfx::blit(
        buf,
        0,
        FONT_CHAR_HEIGHT,
        0,
        0,
        width,
        height - FONT_CHAR_HEIGHT,
    );

    // Clear the bottom line
    gfx::fill_rect(
        buf,
        0,
        height - FONT_CHAR_HEIGHT,
        width,
        FONT_CHAR_HEIGHT,
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

    // Clear character in scrollback
    let slot = display.line_slot(cursor_line);
    if (cursor_col as usize) < SHELL_SCROLLBACK_COLS {
        scrollback::set_char(slot, cursor_col as usize, 0);
        // Trim trailing zeros from line length
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

#[unsafe(link_section = ".user_text")]
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
            if needs_scroll {
                let view_diff = display.view_top.get() - old_view_top;
                if view_diff == 1 {
                    if !scroll_up_fast(buf, display) {
                        redraw_view(buf, display);
                    }
                } else {
                    redraw_view(buf, display);
                }
            } else {
                // Draw all lines from start_line to cursor_line (inclusive)
                // This ensures lines completed by newlines are rendered
                let view_top = display.view_top.get();
                let rows = display.rows.get();
                let cursor_line = display.cursor_line.get();
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

fn console_page_up(display: &DisplayState) {
    if display.total_lines.get() <= display.rows.get() {
        return;
    }

    let step = display.rows.get().max(1);
    let new_top = (display.view_top.get() - step).max(0);
    let delta = (display.view_top.get() - new_top).max(0);

    if delta == 0 {
        return;
    }

    display.view_top.set(new_top);
    display.follow.set(false);

    surface::draw(|buf| {
        let rows = display.rows.get();
        if delta < rows {
            let shift = delta * FONT_CHAR_HEIGHT;
            let width = display.width.get();
            let height = display.height.get();
            gfx::blit(buf, 0, 0, 0, shift, width, height - shift);

            let view_top = display.view_top.get();
            for row in 0..delta {
                draw_row_from_scrollback(buf, display, view_top + row, row);
            }
        } else {
            redraw_view(buf, display);
        }
    });
}

fn console_page_down(display: &DisplayState) {
    let rows = display.rows.get();
    let total_lines = display.total_lines.get();

    if total_lines <= rows {
        return;
    }

    let max_top = (total_lines - rows).max(0);
    let step = rows.max(1);
    let new_top = (display.view_top.get() + step).min(max_top);
    let delta = (new_top - display.view_top.get()).max(0);

    if delta == 0 {
        return;
    }

    display.view_top.set(new_top);
    display.follow.set(new_top == max_top);

    surface::draw(|buf| {
        if delta < rows {
            let shift = delta * FONT_CHAR_HEIGHT;
            let width = display.width.get();
            let height = display.height.get();
            gfx::blit(buf, 0, shift, 0, 0, width, height - shift);

            let start = rows - delta;
            let view_top = display.view_top.get();
            for row in start..rows {
                draw_row_from_scrollback(buf, display, view_top + row, row);
            }
        } else {
            redraw_view(buf, display);
        }
    });
}

fn console_ensure_follow(display: &DisplayState) {
    let max_top = (display.total_lines.get() - display.rows.get()).max(0);
    display.view_top.set(max_top);
    display.follow.set(true);

    surface::draw(|buf| {
        redraw_view(buf, display);
    });
}

fn console_rewrite_input(display: &DisplayState, prompt: &[u8], input: &[u8]) {
    if !display.enabled.get() {
        return;
    }

    let cursor_line = display.cursor_line.get();
    let slot = display.line_slot(cursor_line);
    let cols = display.cols.get() as usize;

    // Build combined line
    let total_len = prompt.len() + input.len();
    let write_len = total_len.min(cols);

    // Write to scrollback
    let mut combined = [0u8; SHELL_SCROLLBACK_COLS];
    let mut idx = 0;
    for &b in prompt.iter().chain(input.iter()).take(write_len) {
        combined[idx] = b;
        idx += 1;
    }
    scrollback::write_line(slot, &combined[..idx]);
    display.cursor_col.set(idx as i32);

    // Render if following
    if display.follow.get() {
        let view_top = display.view_top.get();
        let row = cursor_line - view_top;
        if row >= 0 && row < display.rows.get() {
            surface::draw(|buf| {
                draw_row_from_scrollback(buf, display, cursor_line, row);
            });
        }
    }
}

// =============================================================================
// Public API functions
// =============================================================================

#[unsafe(link_section = ".user_text")]
fn shell_console_init() {
    let mut info = UserFbInfo::default();
    if sys_fb_info(&mut info) != 0 || info.width == 0 || info.height == 0 {
        return;
    }

    let width = SHELL_WINDOW_WIDTH;
    let height = SHELL_WINDOW_HEIGHT;

    // Initialize display state
    DISPLAY.width.set(width);
    DISPLAY.height.set(height);
    let bytes_pp = ((info.bpp as usize + 7) / 8) as u8;
    DISPLAY.bytes_pp.set(bytes_pp);
    DISPLAY.pitch.set((width as usize) * (bytes_pp as usize));

    let cols = width / FONT_CHAR_WIDTH;
    let rows = height / FONT_CHAR_HEIGHT;
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

    // Initialize surface
    if !surface::init(width, height, info.bpp) {
        DISPLAY.enabled.set(false);
        return;
    }

    DISPLAY.enabled.set(true);
    DISPLAY.reset();
    DISPLAY.fg.set(SHELL_FG_COLOR);
    DISPLAY.bg.set(SHELL_BG_COLOR);
}

#[unsafe(link_section = ".user_text")]
fn shell_console_clear() {
    if DISPLAY.enabled.get() {
        console_clear(&DISPLAY);
        let _ = sys_surface_commit();
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_console_write(buf: &[u8]) {
    if DISPLAY.enabled.get() {
        console_write(&DISPLAY, buf);
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_write(buf: &[u8]) {
    let _ = sys_write(buf);
    shell_console_write(buf);
    shell_console_commit();
}

#[unsafe(link_section = ".user_text")]
fn shell_echo_char(c: u8) {
    let buf = [c];
    let _ = sys_write(&buf);
    shell_console_write(&buf);
    shell_console_commit();
}

#[unsafe(link_section = ".user_text")]
fn shell_console_get_cursor() -> (i32, i32) {
    if DISPLAY.enabled.get() {
        DISPLAY.cursor()
    } else {
        (0, 0)
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_console_page_up() {
    if DISPLAY.enabled.get() {
        console_page_up(&DISPLAY);
        let _ = sys_surface_commit();
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_console_page_down() {
    if DISPLAY.enabled.get() {
        console_page_down(&DISPLAY);
        let _ = sys_surface_commit();
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_console_commit() {
    if DISPLAY.enabled.get() {
        let _ = sys_surface_commit();
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_console_follow_bottom() {
    if DISPLAY.enabled.get() {
        console_ensure_follow(&DISPLAY);
        let _ = sys_surface_commit();
    }
}

#[unsafe(link_section = ".user_text")]
fn shell_redraw_input(_line_row: i32, input: &[u8]) {
    if DISPLAY.enabled.get() {
        console_rewrite_input(&DISPLAY, PROMPT, input);
        let _ = sys_surface_commit();
    }
}

// =============================================================================
// Command parsing and builtins
// =============================================================================

type BuiltinFn = fn(argc: i32, argv: &[*const u8]) -> i32;

struct BuiltinEntry {
    name: &'static [u8],
    desc: &'static [u8],
    func: BuiltinFn,
}

#[unsafe(link_section = ".user_rodata")]
static BUILTINS: &[BuiltinEntry] = &[
    BuiltinEntry {
        name: b"help",
        func: cmd_help,
        desc: b"List available commands",
    },
    BuiltinEntry {
        name: b"echo",
        func: cmd_echo,
        desc: b"Print arguments back to the terminal",
    },
    BuiltinEntry {
        name: b"clear",
        func: cmd_clear,
        desc: b"Clear the terminal display",
    },
    BuiltinEntry {
        name: b"shutdown",
        func: cmd_shutdown,
        desc: b"Power off the system",
    },
    BuiltinEntry {
        name: b"info",
        func: cmd_info,
        desc: b"Show kernel memory and scheduler stats",
    },
    BuiltinEntry {
        name: b"ls",
        func: cmd_ls,
        desc: b"List directory contents",
    },
    BuiltinEntry {
        name: b"cat",
        func: cmd_cat,
        desc: b"Display file contents",
    },
    BuiltinEntry {
        name: b"write",
        func: cmd_write,
        desc: b"Write text to a file",
    },
    BuiltinEntry {
        name: b"mkdir",
        func: cmd_mkdir,
        desc: b"Create a directory",
    },
    BuiltinEntry {
        name: b"rm",
        func: cmd_rm,
        desc: b"Remove a file",
    },
];

#[inline(always)]
fn is_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

#[unsafe(link_section = ".user_text")]
fn u_streq_slice(a: *const u8, b: &[u8]) -> bool {
    if a.is_null() {
        return b.is_empty();
    }
    let len = runtime::u_strlen(a);
    if len != b.len() {
        return false;
    }
    for i in 0..len {
        unsafe {
            if *a.add(i) != b[i] {
                return false;
            }
        }
    }
    true
}

#[unsafe(link_section = ".user_text")]
fn normalize_path(input: *const u8, buffer: &mut [u8]) -> i32 {
    if buffer.is_empty() {
        return -1;
    }
    if input.is_null() || unsafe { *input } == 0 {
        buffer[0] = b'/';
        if buffer.len() > 1 {
            buffer[1] = 0;
        }
        return 0;
    }

    unsafe {
        if *input == b'/' {
            let len = runtime::u_strnlen(input, buffer.len().saturating_sub(1));
            if len >= buffer.len() {
                return -1;
            }
            ptr::copy_nonoverlapping(input, buffer.as_mut_ptr(), len);
            buffer[len] = 0;
            return 0;
        }
    }

    let maxlen = buffer.len().saturating_sub(2);
    let len = runtime::u_strnlen(input, maxlen);
    if len > maxlen {
        return -1;
    }
    buffer[0] = b'/';
    unsafe {
        ptr::copy_nonoverlapping(input, buffer.as_mut_ptr().add(1), len);
    }
    let term_idx = cmp::min(len + 1, buffer.len() - 1);
    buffer[term_idx] = 0;
    0
}

#[unsafe(link_section = ".user_text")]
fn shell_parse_line(line: &[u8], tokens: &mut [*const u8]) -> i32 {
    if line.is_empty() || tokens.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut cursor = 0usize;

    while cursor < line.len() {
        while cursor < line.len() && is_space(line[cursor]) {
            cursor += 1;
        }
        if cursor >= line.len() || line[cursor] == 0 {
            break;
        }
        let start = cursor;
        while cursor < line.len() && line[cursor] != 0 && !is_space(line[cursor]) {
            cursor += 1;
        }
        if count >= tokens.len() {
            continue;
        }
        let token_len = cmp::min(cursor - start, SHELL_MAX_TOKEN_LENGTH - 1);

        buffers::with_token_storage(|storage| {
            storage[count][..token_len].copy_from_slice(&line[start..start + token_len]);
            storage[count][token_len] = 0;
        });
        tokens[count] = buffers::token_ptr(count);
        count += 1;
    }

    if count < tokens.len() {
        tokens[count] = ptr::null();
    }
    count as i32
}

#[unsafe(link_section = ".user_text")]
fn find_builtin(name: *const u8) -> Option<&'static BuiltinEntry> {
    for entry in BUILTINS {
        if u_streq_slice(name, entry.name) {
            return Some(entry);
        }
    }
    None
}

#[unsafe(link_section = ".user_text")]
fn print_kv(key: &[u8], value: u64) {
    if !key.is_empty() {
        shell_write(key);
    }
    let mut tmp = [0u8; 32];
    let mut idx = 0usize;
    if value == 0 {
        tmp[idx] = b'0';
        idx += 1;
    } else {
        let mut n = value;
        let mut rev = [0u8; 32];
        let mut r = 0usize;
        while n != 0 && r < rev.len() {
            rev[r] = b'0' + (n % 10) as u8;
            n /= 10;
            r += 1;
        }
        while r > 0 && idx < tmp.len() {
            idx += 1;
            tmp[idx - 1] = rev[r - 1];
            r -= 1;
        }
    }
    shell_write(&tmp[..idx]);
    shell_write(NL);
}

// =============================================================================
// Built-in commands
// =============================================================================

#[unsafe(link_section = ".user_text")]
fn cmd_help(_argc: i32, _argv: &[*const u8]) -> i32 {
    shell_write(HELP_HEADER);
    for entry in BUILTINS {
        shell_write(b"  ");
        shell_write(entry.name);
        shell_write(b" - ");
        if !entry.desc.is_empty() {
            shell_write(entry.desc);
        }
        shell_write(NL);
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_echo(argc: i32, argv: &[*const u8]) -> i32 {
    let mut first = true;
    for i in 1..argc {
        let idx = i as usize;
        if idx >= argv.len() {
            break;
        }
        let arg = argv[idx];
        if arg.is_null() {
            continue;
        }
        if !first {
            shell_write(b" ");
        }
        let len = runtime::u_strlen(arg);
        shell_write(unsafe { core::slice::from_raw_parts(arg, len) });
        first = false;
    }
    shell_write(NL);
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_clear(_argc: i32, _argv: &[*const u8]) -> i32 {
    shell_write(b"\x1B[2J\x1B[H");
    shell_console_clear();
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_shutdown(_argc: i32, _argv: &[*const u8]) -> i32 {
    shell_write(HALTED);
    sys_halt();
}

#[unsafe(link_section = ".user_text")]
fn cmd_info(_argc: i32, _argv: &[*const u8]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_sys_info(&mut info) != 0 {
        shell_write(b"info: failed\n");
        return 1;
    }
    shell_write(b"Kernel information:\n");
    shell_write(b"  Memory: total pages=");
    print_kv(b"", info.total_pages as u64);
    shell_write(b"  Free pages=");
    print_kv(b"", info.free_pages as u64);
    shell_write(b"  Allocated pages=");
    print_kv(b"", info.allocated_pages as u64);
    shell_write(b"  Tasks: total=");
    print_kv(b"", info.total_tasks as u64);
    shell_write(b"  Active tasks=");
    print_kv(b"", info.active_tasks as u64);
    shell_write(b"  Task ctx switches=");
    print_kv(b"", info.task_context_switches);
    shell_write(b"  Scheduler: switches=");
    print_kv(b"", info.scheduler_context_switches);
    shell_write(b"  Yields=");
    print_kv(b"", info.scheduler_yields);
    shell_write(b"  Ready=");
    print_kv(b"", info.ready_tasks as u64);
    shell_write(b"  schedule() calls=");
    print_kv(b"", info.schedule_calls as u64);
    0
}

#[unsafe(link_section = ".user_text")]
fn cmd_ls(argc: i32, argv: &[*const u8]) -> i32 {
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    let path_ptr = if argc == 2 { argv[1] } else { ptr::null() };

    let path = buffers::with_path_buf(|path_buf| {
        if path_ptr.is_null() {
            b"/\0".as_ptr()
        } else {
            if normalize_path(path_ptr, path_buf) != 0 {
                shell_write(PATH_TOO_LONG);
                return ptr::null();
            }
            path_buf.as_ptr()
        }
    });

    if path.is_null() {
        return 1;
    }

    let result = buffers::with_list_entries(|entries| {
        let mut list = UserFsList {
            entries: entries.as_mut_ptr(),
            max_entries: entries.len() as u32,
            count: 0,
        };

        if unsafe { sys_fs_list(path as *const c_char, &mut list) } != 0 {
            shell_write(ERR_NO_SUCH);
            return 1;
        }

        for i in 0..list.count {
            let entry = &entries[i as usize];
            if entry.is_directory() {
                shell_write(b"[");
                shell_write(
                    &entry.name[..runtime::u_strnlen(entry.name.as_ptr(), entry.name.len())],
                );
                shell_write(b"]\n");
            } else {
                let name_len = runtime::u_strnlen(entry.name.as_ptr(), entry.name.len());
                shell_write(&entry.name[..name_len]);
                shell_write(b" (");
                print_kv(b"", entry.size as u64);
            }
        }
        0
    });

    result
}

#[unsafe(link_section = ".user_text")]
fn cmd_cat(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }

        let mut tmp = [0u8; SHELL_IO_MAX + 1];
        let fd = unsafe { sys_fs_open(path_buf.as_ptr() as *const c_char, USER_FS_OPEN_READ) };
        if fd < 0 {
            shell_write(ERR_NO_SUCH);
            return 1;
        }
        let r = unsafe { sys_fs_read(fd as i32, tmp.as_mut_ptr() as *mut c_void, SHELL_IO_MAX) };
        let _ = sys_fs_close(fd as i32);
        if r < 0 {
            shell_write(ERR_NO_SUCH);
            return 1;
        }
        let len = cmp::min(r as usize, tmp.len() - 1);
        tmp[len] = 0;
        shell_write(&tmp[..len]);
        if r as usize == SHELL_IO_MAX {
            shell_write(b"\n[truncated]\n");
        }
        0
    })
}

#[unsafe(link_section = ".user_text")]
fn cmd_write(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_FILE);
        return 1;
    }
    if argc < 3 {
        shell_write(ERR_MISSING_TEXT);
        return 1;
    }
    if argc > 3 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }

        let text = argv[2];
        if text.is_null() {
            shell_write(ERR_MISSING_TEXT);
            return 1;
        }

        let mut len = runtime::u_strlen(text);
        if len > SHELL_IO_MAX {
            len = SHELL_IO_MAX;
        }

        let fd = unsafe {
            sys_fs_open(
                path_buf.as_ptr() as *const c_char,
                USER_FS_OPEN_WRITE | USER_FS_OPEN_CREAT,
            )
        };
        if fd < 0 {
            shell_write(b"write failed\n");
            return 1;
        }
        let w = unsafe { sys_fs_write(fd as i32, text as *const c_void, len) };
        let _ = sys_fs_close(fd as i32);
        if w < 0 || w as usize != len {
            shell_write(b"write failed\n");
            return 1;
        }
        0
    })
}

#[unsafe(link_section = ".user_text")]
fn cmd_mkdir(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_OPERAND);
        return 1;
    }
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }
        if unsafe { sys_fs_mkdir(path_buf.as_ptr() as *const c_char) } != 0 {
            shell_write(b"mkdir failed\n");
            return 1;
        }
        0
    })
}

#[unsafe(link_section = ".user_text")]
fn cmd_rm(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write(ERR_MISSING_OPERAND);
        return 1;
    }
    if argc > 2 {
        shell_write(ERR_TOO_MANY_ARGS);
        return 1;
    }

    buffers::with_path_buf(|path_buf| {
        if normalize_path(argv[1], path_buf) != 0 {
            shell_write(PATH_TOO_LONG);
            return 1;
        }
        if unsafe { sys_fs_unlink(path_buf.as_ptr() as *const c_char) } != 0 {
            shell_write(b"rm failed\n");
            return 1;
        }
        0
    })
}

// =============================================================================
// Main entry point
// =============================================================================

#[unsafe(link_section = ".user_text")]
pub fn shell_user_main(_arg: *mut c_void) {
    shell_console_init();
    shell_console_clear();

    // Set window title
    sys_surface_set_title("SlopOS Shell");

    shell_write(WELCOME);

    loop {
        let (_, line_row) = shell_console_get_cursor();
        shell_write(PROMPT);

        // Clear line buffer
        buffers::with_line_buf(|buf| {
            runtime::u_memset(buf.as_mut_ptr() as *mut c_void, 0, buf.len());
        });

        let mut len = 0usize;

        loop {
            let rc = sys_read_char();
            if rc < 0 {
                continue;
            }
            let c = rc as u8;

            if c == KEY_PAGE_UP {
                shell_console_page_up();
                continue;
            }
            if c == KEY_PAGE_DOWN {
                shell_console_page_down();
                continue;
            }

            // Return to follow mode if we were scrolled up
            if DISPLAY.enabled.get() && !DISPLAY.follow.get() {
                shell_console_follow_bottom();
            }

            if c == b'\n' || c == b'\r' {
                shell_echo_char(b'\n');
                break;
            }

            if c == b'\x08' || c == 0x7f {
                if len > 0 {
                    len -= 1;
                    buffers::with_line_buf(|buf| {
                        shell_redraw_input(line_row, &buf[..len]);
                    });
                }
                continue;
            }

            if c < 0x20 {
                continue;
            }

            let max_len = buffers::with_line_buf(|buf| buf.len());
            if len + 1 >= max_len {
                continue;
            }

            buffers::with_line_buf(|buf| {
                buf[len] = c;
            });
            len += 1;

            buffers::with_line_buf(|buf| {
                shell_redraw_input(line_row, &buf[..len]);
            });
        }

        // Null-terminate
        buffers::with_line_buf(|buf| {
            let capped = cmp::min(len, buf.len() - 1);
            buf[capped] = 0;
        });

        // Parse and execute
        let mut tokens: [*const u8; SHELL_MAX_TOKENS] = [ptr::null(); SHELL_MAX_TOKENS];
        let token_count = buffers::with_line_buf(|buf| shell_parse_line(buf, &mut tokens));

        if token_count <= 0 {
            continue;
        }

        let builtin = find_builtin(tokens[0]);
        if let Some(b) = builtin {
            (b.func)(token_count, &tokens);
        } else {
            shell_write(UNKNOWN_CMD);
        }
    }
}
