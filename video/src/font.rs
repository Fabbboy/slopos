use core::ffi::{c_char, c_int};
use core::ptr;

use crate::framebuffer::{self, FbState};
use crate::graphics::GraphicsContext;

pub(crate) use slopos_abi::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH};

const FONT_SUCCESS: c_int = 0;
const FONT_ERROR_NO_FB: c_int = -1;
const FONT_ERROR_INVALID: c_int = -3;

struct ConsoleState {
    cursor_x: i32,
    cursor_y: i32,
    fg_color: u32,
    bg_color: u32,
    initialized: bool,
}

impl ConsoleState {
    pub const fn new() -> Self {
        Self {
            cursor_x: 0,
            cursor_y: 0,
            fg_color: 0xFFFF_FFFF,
            bg_color: 0x0000_0000,
            initialized: false,
        }
    }
}

static FONT_CONSOLE: spin::Mutex<ConsoleState> = spin::Mutex::new(const { ConsoleState::new() });

fn framebuffer_ready() -> bool {
    framebuffer::framebuffer_is_initialized() != 0
}

fn glyph_for_char(c: c_char) -> &'static [u8; FONT_CHAR_HEIGHT as usize] {
    slopos_abi::font::get_glyph_or_space((c as i8) as u8)
}

fn bytes_per_pixel(bpp: u8) -> u32 {
    ((bpp as u32) + 7) / 8
}

fn convert_color(fb: &FbState, color: u32) -> u32 {
    match fb.pixel_format {
        0x02 | 0x04 => {
            ((color & 0xFF0000) >> 16)
                | (color & 0x00FF00)
                | ((color & 0x0000FF) << 16)
                | (color & 0xFF000000)
        }
        _ => color,
    }
}

#[inline]
fn write_pixel(fb: &FbState, x: i32, y: i32, pixel_value: u32) {
    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let offset = y as usize * fb.pitch as usize + x as usize * bytes_pp;
    let pixel_ptr = unsafe { fb.base.add(offset) };

    unsafe {
        match bytes_pp {
            2 => ptr::write_volatile(pixel_ptr as *mut u16, pixel_value as u16),
            3 => {
                ptr::write_volatile(pixel_ptr, ((pixel_value >> 16) & 0xFF) as u8);
                ptr::write_volatile(pixel_ptr.add(1), ((pixel_value >> 8) & 0xFF) as u8);
                ptr::write_volatile(pixel_ptr.add(2), (pixel_value & 0xFF) as u8);
            }
            4 => ptr::write_volatile(pixel_ptr as *mut u32, pixel_value),
            _ => {}
        }
    }
}

fn fill_rect_fb(fb: &FbState, x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }

    let mut x1 = x;
    let mut y1 = y;
    let mut x2 = x + w - 1;
    let mut y2 = y + h - 1;

    if x1 < 0 {
        x1 = 0;
    }
    if y1 < 0 {
        y1 = 0;
    }
    if x2 >= fb.width as i32 {
        x2 = fb.width as i32 - 1;
    }
    if y2 >= fb.height as i32 {
        y2 = fb.height as i32 - 1;
    }

    if x1 > x2 || y1 > y2 {
        return;
    }

    let pixel_value = convert_color(fb, color);
    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let pitch = fb.pitch as usize;

    for row in y1..=y2 {
        let mut pixel_ptr = unsafe { fb.base.add(row as usize * pitch + x1 as usize * bytes_pp) };
        if bytes_pp == 4 {
            let mut count = x2 - x1 + 1;
            while count > 0 {
                unsafe {
                    (pixel_ptr as *mut u32).write_volatile(pixel_value);
                    pixel_ptr = pixel_ptr.add(bytes_pp);
                }
                count -= 1;
            }
        } else {
            for _ in x1..=x2 {
                unsafe {
                    match bytes_pp {
                        2 => (pixel_ptr as *mut u16).write_volatile(pixel_value as u16),
                        3 => {
                            ptr::write_volatile(pixel_ptr, ((pixel_value >> 16) & 0xFF) as u8);
                            ptr::write_volatile(
                                pixel_ptr.add(1),
                                ((pixel_value >> 8) & 0xFF) as u8,
                            );
                            ptr::write_volatile(pixel_ptr.add(2), (pixel_value & 0xFF) as u8);
                        }
                        _ => {}
                    }
                    pixel_ptr = pixel_ptr.add(bytes_pp);
                }
            }
        }
    }
}

pub fn font_draw_char_ctx(
    ctx: &GraphicsContext,
    x: i32,
    y: i32,
    c: c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return FONT_ERROR_NO_FB,
    };

    let fb_w = ctx.width() as i32;
    let fb_h = ctx.height() as i32;
    let glyph = glyph_for_char(c);
    let fg_raw = convert_color(&fb, fg_color);
    let bg_raw = convert_color(&fb, bg_color);

    for (row_idx, byte) in glyph.iter().copied().enumerate() {
        let py = y + row_idx as i32;
        if py < 0 || py >= fb_h {
            continue;
        }
        for col in 0..FONT_CHAR_WIDTH {
            let px = x + col;
            if px < 0 || px >= fb_w {
                continue;
            }
            if byte & (0x80 >> col) != 0 {
                write_pixel(&fb, px, py, fg_raw);
            } else if bg_color != 0 {
                write_pixel(&fb, px, py, bg_raw);
            }
        }
    }

    FONT_SUCCESS
}

unsafe fn c_str_len(ptr: *const c_char) -> usize {
    if ptr.is_null() {
        return 0;
    }
    let mut len = 0usize;
    let mut p = ptr;
    unsafe {
        while *p != 0 {
            len += 1;
            p = p.add(1);
        }
    }
    len
}

unsafe fn c_str_to_bytes<'a>(ptr: *const c_char, buf: &'a mut [u8]) -> &'a [u8] {
    if ptr.is_null() {
        return &[];
    }
    let len = unsafe { c_str_len(ptr) }.min(buf.len());
    for i in 0..len {
        unsafe {
            buf[i] = *ptr.add(i) as u8;
        }
    }
    &buf[..len]
}

pub fn font_draw_string_ctx(
    ctx: &GraphicsContext,
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    if str_ptr.is_null() {
        return FONT_ERROR_INVALID;
    }

    let fb_w = ctx.width() as i32;
    let fb_h = ctx.height() as i32;
    let mut cx = x;
    let mut cy = y;
    let mut tmp = [0u8; 1024];
    let text = unsafe { c_str_to_bytes(str_ptr, &mut tmp) };

    for &ch in text {
        let c = ch as c_char;
        match ch {
            b'\n' => {
                cx = x;
                cy += FONT_CHAR_HEIGHT;
            }
            b'\r' => {
                cx = x;
            }
            b'\t' => {
                let tab_width = 4 * FONT_CHAR_WIDTH;
                cx = ((cx - x + tab_width) / tab_width) * tab_width + x;
            }
            _ => {
                font_draw_char_ctx(ctx, cx, cy, c, fg_color, bg_color);
                cx += FONT_CHAR_WIDTH;
                if cx + FONT_CHAR_WIDTH > fb_w {
                    cx = x;
                    cy += FONT_CHAR_HEIGHT;
                }
            }
        }

        if cy >= fb_h {
            break;
        }
    }

    FONT_SUCCESS
}

pub fn font_draw_string_clear_ctx(
    ctx: &GraphicsContext,
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    if str_ptr.is_null() {
        return FONT_ERROR_INVALID;
    }

    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return FONT_ERROR_NO_FB,
    };

    let width = font_get_string_width(str_ptr);
    let height = FONT_CHAR_HEIGHT;
    fill_rect_fb(&fb, x, y, width, height, bg_color);
    font_draw_string_ctx(ctx, x, y, str_ptr, fg_color, bg_color)
}

pub fn font_get_string_width(str_ptr: *const c_char) -> i32 {
    if str_ptr.is_null() {
        return 0;
    }
    let mut width = 0;
    let mut p = str_ptr;
    loop {
        let ch = unsafe { *p };
        if ch == 0 || ch == b'\n' as c_char {
            break;
        }
        if ch == b'\t' as c_char {
            let tab_width = 4 * FONT_CHAR_WIDTH;
            width = ((width + tab_width - 1) / tab_width) * tab_width;
        } else {
            width += FONT_CHAR_WIDTH;
        }
        unsafe {
            p = p.add(1);
        }
    }
    width
}

pub fn font_get_string_lines(str_ptr: *const c_char) -> c_int {
    if str_ptr.is_null() {
        return 0;
    }
    let mut lines = 1;
    let mut p = str_ptr;
    loop {
        let ch = unsafe { *p };
        if ch == 0 {
            break;
        }
        if ch == b'\n' as c_char {
            lines += 1;
        }
        unsafe {
            p = p.add(1);
        }
    }
    lines
}

fn console_scroll_up(state: &mut ConsoleState) {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return,
    };

    let bpp_bytes = ((fb.bpp as usize) + 7) / 8;
    if bpp_bytes == 0 || fb.pitch == 0 {
        return;
    }

    if fb.height <= FONT_CHAR_HEIGHT as u32 {
        fill_rect_fb(&fb, 0, 0, fb.width as i32, fb.height as i32, state.bg_color);
        state.cursor_y = 0;
        return;
    }

    let src_offset = FONT_CHAR_HEIGHT as usize * fb.pitch as usize;
    let copy_bytes = (fb.height as usize - FONT_CHAR_HEIGHT as usize) * fb.pitch as usize;

    unsafe {
        ptr::copy(fb.base.add(src_offset), fb.base, copy_bytes);
    }

    fill_rect_fb(
        &fb,
        0,
        fb.height as i32 - FONT_CHAR_HEIGHT,
        fb.width as i32,
        FONT_CHAR_HEIGHT,
        state.bg_color,
    );
    state.cursor_y = fb.height as i32 - FONT_CHAR_HEIGHT;
}

pub fn font_console_init(fg_color: u32, bg_color: u32) {
    let mut console = FONT_CONSOLE.lock();
    console.cursor_x = 0;
    console.cursor_y = 0;
    console.fg_color = fg_color;
    console.bg_color = bg_color;
    console.initialized = true;
}

fn console_putc_with_ctx(ctx: &GraphicsContext, console: &mut ConsoleState, c: c_char) {
    match c as u8 {
        b'\n' => {
            console.cursor_x = 0;
            console.cursor_y += FONT_CHAR_HEIGHT;
        }
        b'\r' => {
            console.cursor_x = 0;
        }
        _ => {
            font_draw_char_ctx(
                ctx,
                console.cursor_x,
                console.cursor_y,
                c,
                console.fg_color,
                console.bg_color,
            );
            console.cursor_x += FONT_CHAR_WIDTH;
            if console.cursor_x + FONT_CHAR_WIDTH > ctx.width() as i32 {
                console.cursor_x = 0;
                console.cursor_y += FONT_CHAR_HEIGHT;
            }
        }
    }

    if console.cursor_y + FONT_CHAR_HEIGHT > ctx.height() as i32 {
        console_scroll_up(console);
    }
}

pub fn font_console_putc(c: c_char) -> c_int {
    if !framebuffer_ready() {
        return FONT_ERROR_NO_FB;
    }

    let ctx = match GraphicsContext::new() {
        Ok(ctx) => ctx,
        Err(_) => return FONT_ERROR_NO_FB,
    };

    let mut console = FONT_CONSOLE.lock();
    if !console.initialized {
        return FONT_ERROR_NO_FB;
    }

    console_putc_with_ctx(&ctx, &mut console, c);
    FONT_SUCCESS
}

pub fn font_console_puts(str_ptr: *const c_char) -> c_int {
    if str_ptr.is_null() {
        return FONT_ERROR_INVALID;
    }
    if !framebuffer_ready() {
        return FONT_ERROR_NO_FB;
    }
    let ctx = match GraphicsContext::new() {
        Ok(ctx) => ctx,
        Err(_) => return FONT_ERROR_NO_FB,
    };
    let mut console = FONT_CONSOLE.lock();
    if !console.initialized {
        return FONT_ERROR_NO_FB;
    }
    let mut p = str_ptr;
    loop {
        let ch = unsafe { *p };
        if ch == 0 {
            break;
        }
        console_putc_with_ctx(&ctx, &mut console, ch);
        unsafe {
            p = p.add(1);
        }
    }
    FONT_SUCCESS
}

pub fn font_console_clear() -> c_int {
    if !framebuffer_ready() {
        return FONT_ERROR_NO_FB;
    }

    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return FONT_ERROR_NO_FB,
    };

    let mut console = FONT_CONSOLE.lock();
    fill_rect_fb(
        &fb,
        0,
        0,
        fb.width as i32,
        fb.height as i32,
        console.bg_color,
    );
    console.cursor_x = 0;
    console.cursor_y = 0;
    FONT_SUCCESS
}

pub fn font_console_set_colors(fg_color: u32, bg_color: u32) {
    let mut console = FONT_CONSOLE.lock();
    console.fg_color = fg_color;
    console.bg_color = bg_color;
}
