
use core::ffi::{c_char, c_void};

use slopos_video::roulette_core::{roulette_run, RouletteBackend};

const SYSCALL_EXIT: u64 = 1;
const SYSCALL_WRITE: u64 = 2;
const SYSCALL_ROULETTE: u64 = 4;
const SYSCALL_SLEEP_MS: u64 = 5;
const SYSCALL_FB_INFO: u64 = 6;
const SYSCALL_GFX_FILL_RECT: u64 = 7;
const SYSCALL_GFX_DRAW_LINE: u64 = 8;
const SYSCALL_GFX_DRAW_CIRCLE: u64 = 9;
const SYSCALL_GFX_DRAW_CIRCLE_FILLED: u64 = 10;
const SYSCALL_FONT_DRAW: u64 = 11;
const SYSCALL_ROULETTE_RESULT: u64 = 13;

#[repr(C)]
#[derive(Copy, Clone)]
struct UserFbInfo {
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u8,
    pixel_format: u8,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UserRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    color: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UserLine {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UserCircle {
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UserText {
    x: i32,
    y: i32,
    fg_color: u32,
    bg_color: u32,
    str_ptr: *const c_char,
    len: u32,
}

impl Default for UserFbInfo {
    #[unsafe(link_section = ".user_text")]
    fn default() -> Self {
        Self { width: 0, height: 0, pitch: 0, bpp: 0, pixel_format: 0 }
    }
}

impl Default for UserRect {
    #[unsafe(link_section = ".user_text")]
    fn default() -> Self {
        Self { x: 0, y: 0, width: 0, height: 0, color: 0 }
    }
}

impl Default for UserLine {
    #[unsafe(link_section = ".user_text")]
    fn default() -> Self {
        Self { x0: 0, y0: 0, x1: 0, y1: 0, color: 0 }
    }
}

impl Default for UserCircle {
    #[unsafe(link_section = ".user_text")]
    fn default() -> Self {
        Self { cx: 0, cy: 0, radius: 0, color: 0 }
    }
}

impl Default for UserText {
    #[unsafe(link_section = ".user_text")]
    fn default() -> Self {
        Self { x: 0, y: 0, fg_color: 0, bg_color: 0, str_ptr: core::ptr::null(), len: 0 }
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
unsafe fn syscall(num: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let mut ret = num;
    unsafe {
        core::arch::asm!(
            "mov {a0}, rdi",
            "mov {a1}, rsi",
            "mov {a2}, rdx",
            "int 0x80",
            a0 = in(reg) arg0,
            a1 = in(reg) arg1,
            a2 = in(reg) arg2,
            inout("rax") ret,
            lateout("rcx") _,
            lateout("r8") _,
            lateout("r9") _,
            lateout("r10") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        );
    }
    ret
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_write(buf: &[u8]) -> i64 {
    unsafe { syscall(SYSCALL_WRITE, buf.as_ptr() as u64, buf.len() as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_sleep_ms(ms: u32) {
    unsafe {
        syscall(SYSCALL_SLEEP_MS, ms as u64, 0, 0);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_roulette() -> u64 {
    unsafe { syscall(SYSCALL_ROULETTE, 0, 0, 0) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_roulette_result(fate_packed: u64) {
    unsafe {
        syscall(SYSCALL_ROULETTE_RESULT, fate_packed, 0, 0);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_exit() -> ! {
    unsafe {
        syscall(SYSCALL_EXIT, 0, 0, 0);
    }
    loop {}
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_fb_info(out: &mut UserFbInfo) -> i64 {
    unsafe { syscall(SYSCALL_FB_INFO, out as *mut _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_gfx_fill_rect(rect: &UserRect) -> i64 {
    unsafe { syscall(SYSCALL_GFX_FILL_RECT, rect as *const _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_gfx_draw_line(line: &UserLine) -> i64 {
    unsafe { syscall(SYSCALL_GFX_DRAW_LINE, line as *const _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_gfx_draw_circle(circle: &UserCircle) -> i64 {
    unsafe { syscall(SYSCALL_GFX_DRAW_CIRCLE, circle as *const _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_gfx_draw_circle_filled(circle: &UserCircle) -> i64 {
    unsafe { syscall(SYSCALL_GFX_DRAW_CIRCLE_FILLED, circle as *const _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
fn sys_font_draw(text: &UserText) -> i64 {
    unsafe { syscall(SYSCALL_FONT_DRAW, text as *const _ as u64, 0, 0) as i64 }
}

#[unsafe(link_section = ".user_text")]
extern "C" fn user_get_size(_ctx: *mut c_void, w: *mut i32, h: *mut i32) -> i32 {
    let mut info = UserFbInfo::default();
    if sys_fb_info(&mut info) != 0 || info.width == 0 || info.height == 0 {
        return -1;
    }
    unsafe {
        if !w.is_null() {
            *w = info.width as i32;
        }
        if !h.is_null() {
            *h = info.height as i32;
        }
    }
    0
}

#[unsafe(link_section = ".user_text")]
extern "C" fn user_fill_rect(
    _ctx: *mut c_void,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) -> i32 {
    let rect = UserRect { x, y, width: w, height: h, color };
    sys_gfx_fill_rect(&rect) as i32
}

#[unsafe(link_section = ".user_text")]
extern "C" fn user_draw_line(
    _ctx: *mut c_void,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> i32 {
    let line = UserLine { x0, y0, x1, y1, color };
    sys_gfx_draw_line(&line) as i32
}

#[unsafe(link_section = ".user_text")]
extern "C" fn user_draw_circle(
    _ctx: *mut c_void,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> i32 {
    let circle = UserCircle { cx, cy, radius, color };
    sys_gfx_draw_circle(&circle) as i32
}

#[unsafe(link_section = ".user_text")]
extern "C" fn user_draw_circle_filled(
    _ctx: *mut c_void,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> i32 {
    let circle = UserCircle { cx, cy, radius, color };
    sys_gfx_draw_circle_filled(&circle) as i32
}

#[unsafe(link_section = ".user_text")]
extern "C" fn user_draw_text(
    _ctx: *mut c_void,
    x: i32,
    y: i32,
    text: *const u8,
    fg: u32,
    bg: u32,
) -> i32 {
    if text.is_null() {
        return -1;
    }
    let mut buf = [0u8; 128];
    let mut len = 0usize;
    unsafe {
        let mut p = text;
        while len < buf.len() - 1 {
            let ch = *p;
            if ch == 0 {
                break;
            }
            buf[len] = ch;
            len += 1;
            p = p.add(1);
        }
    }
    let text_desc = UserText {
        x,
        y,
        fg_color: fg,
        bg_color: bg,
        str_ptr: buf.as_ptr() as *const c_char,
        len: len as u32,
    };
    sys_font_draw(&text_desc) as i32
}

#[unsafe(link_section = ".user_text")]
extern "C" fn user_sleep_ms(_ctx: *mut c_void, ms: u32) {
    sys_sleep_ms(ms);
}

#[unsafe(link_section = ".user_text")]
fn text_fallback(fate: u32) {
    const HDR: &[u8] = b"ROULETTE: framebuffer unavailable, using text fallback\n";
    const LBL: &[u8] = b"Fate number: ";
    sys_write(HDR);
    sys_write(LBL);

    let mut digits = [0u8; 32];
    let mut idx = 0usize;
    if fate == 0 {
        digits[idx] = b'0';
        idx += 1;
    } else {
        let mut n = fate;
        let mut tmp = [0u8; 32];
        let mut t = 0usize;
        while n != 0 && t < tmp.len() {
            tmp[t] = b'0' + (n % 10) as u8;
            n /= 10;
            t += 1;
        }
        while t > 0 {
            idx += 1;
            digits[idx - 1] = tmp[t - 1];
            t -= 1;
        }
    }
    sys_write(&digits[..idx]);
    sys_write(b"\n");
}

#[unsafe(link_section = ".user_text")]
fn backend() -> RouletteBackend {
    RouletteBackend {
        ctx: core::ptr::null_mut(),
        get_size: Some(user_get_size),
        fill_rect: Some(user_fill_rect),
        draw_line: Some(user_draw_line),
        draw_circle: Some(user_draw_circle),
        draw_circle_filled: Some(user_draw_circle_filled),
        draw_text: Some(user_draw_text),
        sleep_ms: Some(user_sleep_ms),
    }
}

#[unsafe(no_mangle)]
#[unsafe(link_section = ".user_text")]
pub extern "C" fn roulette_user_main(_arg: *mut c_void) {
    let _ = sys_write(b"ROULETTE: start\n");
    let spin = sys_roulette();
    let fate = spin as u32;

    let mut info = UserFbInfo::default();
    let fb_rc = sys_fb_info(&mut info);
    let fb_ok = fb_rc == 0 && info.width != 0 && info.height != 0;
    let mut rc = -1;

    if !fb_ok {
        let _ = sys_write(b"ROULETTE: fb_info failed, falling back to text\n");
        text_fallback(fate);
    } else {
        let _ = sys_write(b"ROULETTE: fb_info ok, drawing wheel\n");
        let backend = backend();
        rc = roulette_run(&backend as *const RouletteBackend, fate);
        if rc != 0 {
            let _ = sys_write(b"ROULETTE: draw failed, falling back to text\n");
            text_fallback(fate);
        }
    }

    let _ = rc;
    sys_sleep_ms(3000);
    sys_roulette_result(spin);
    sys_sleep_ms(500);
    sys_exit();
}
