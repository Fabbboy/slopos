use core::ffi::{c_char, c_int};
use core::sync::atomic::{AtomicPtr, Ordering};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FramebufferInfoC {
    pub initialized: u8,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub pixel_format: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VideoCallbacks {
    pub draw_rect_filled_fast: Option<fn(i32, i32, i32, i32, u32) -> i32>,
    pub draw_line: Option<fn(i32, i32, i32, i32, u32) -> i32>,
    pub draw_circle: Option<fn(i32, i32, i32, u32) -> i32>,
    pub draw_circle_filled: Option<fn(i32, i32, i32, u32) -> i32>,
    pub font_draw_string: Option<fn(i32, i32, *const c_char, u32, u32) -> c_int>,
    pub framebuffer_get_info: Option<fn() -> *mut FramebufferInfoC>,
}

static mut VIDEO_CALLBACKS: VideoCallbacks = VideoCallbacks {
    draw_rect_filled_fast: None,
    draw_line: None,
    draw_circle: None,
    draw_circle_filled: None,
    font_draw_string: None,
    framebuffer_get_info: None,
};

static FRAMEBUFFER_INFO: AtomicPtr<FramebufferInfoC> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_video_callbacks(callbacks: VideoCallbacks) {
    unsafe {
        VIDEO_CALLBACKS = callbacks;
    }
}

pub fn draw_rect_filled_fast(x: i32, y: i32, w: i32, h: i32, color: u32) -> i32 {
    unsafe {
        if let Some(cb) = VIDEO_CALLBACKS.draw_rect_filled_fast {
            return cb(x, y, w, h, color);
        }
    }
    -1
}

pub fn draw_line(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) -> i32 {
    unsafe {
        if let Some(cb) = VIDEO_CALLBACKS.draw_line {
            return cb(x0, y0, x1, y1, color);
        }
    }
    -1
}

pub fn draw_circle(cx: i32, cy: i32, radius: i32, color: u32) -> i32 {
    unsafe {
        if let Some(cb) = VIDEO_CALLBACKS.draw_circle {
            return cb(cx, cy, radius, color);
        }
    }
    -1
}

pub fn draw_circle_filled(cx: i32, cy: i32, radius: i32, color: u32) -> i32 {
    unsafe {
        if let Some(cb) = VIDEO_CALLBACKS.draw_circle_filled {
            return cb(cx, cy, radius, color);
        }
    }
    -1
}

pub fn font_draw_string(
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    unsafe {
        if let Some(cb) = VIDEO_CALLBACKS.font_draw_string {
            return cb(x, y, str_ptr, fg_color, bg_color);
        }
    }
    -1
}

pub fn framebuffer_get_info() -> *mut FramebufferInfoC {
    if let Some(cb) = unsafe { VIDEO_CALLBACKS.framebuffer_get_info } {
        let ptr = cb();
        FRAMEBUFFER_INFO.store(ptr, Ordering::Relaxed);
        return ptr;
    }
    FRAMEBUFFER_INFO.load(Ordering::Relaxed)
}
