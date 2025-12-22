use core::ffi::{c_char, c_int};
use core::sync::atomic::{AtomicPtr, Ordering};
use spin::Once;

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
pub struct WindowInfo {
    pub task_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub state: u8,
    pub title: [c_char; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoError {
    NoFramebuffer,
    OutOfBounds,
    Invalid,
}

pub type VideoResult = Result<(), VideoError>;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VideoCallbacks {
    pub draw_rect_filled_fast: Option<fn(i32, i32, i32, i32, u32) -> c_int>,
    pub draw_line: Option<fn(i32, i32, i32, i32, u32) -> c_int>,
    pub draw_circle: Option<fn(i32, i32, i32, u32) -> c_int>,
    pub draw_circle_filled: Option<fn(i32, i32, i32, u32) -> c_int>,
    pub font_draw_string: Option<fn(i32, i32, *const c_char, u32, u32) -> c_int>,
    pub framebuffer_blit: Option<fn(i32, i32, i32, i32, i32, i32) -> c_int>,
    pub framebuffer_get_info: Option<fn() -> *mut FramebufferInfoC>,
    pub roulette_draw: Option<fn(u32) -> c_int>,
    pub surface_draw_rect_filled_fast: Option<fn(u32, i32, i32, i32, i32, u32) -> c_int>,
    pub surface_draw_line: Option<fn(u32, i32, i32, i32, i32, u32) -> c_int>,
    pub surface_draw_circle: Option<fn(u32, i32, i32, i32, u32) -> c_int>,
    pub surface_draw_circle_filled: Option<fn(u32, i32, i32, i32, u32) -> c_int>,
    pub surface_font_draw_string: Option<fn(u32, i32, i32, *const c_char, u32, u32) -> c_int>,
    pub surface_blit: Option<fn(u32, i32, i32, i32, i32, i32, i32) -> c_int>,
    pub compositor_present: Option<fn() -> c_int>,
    pub surface_enumerate_windows: Option<fn(*mut WindowInfo, u32) -> u32>,
    pub surface_set_window_position: Option<fn(u32, i32, i32) -> c_int>,
    pub surface_set_window_state: Option<fn(u32, u8) -> c_int>,
    pub surface_raise_window: Option<fn(u32) -> c_int>,
}

static VIDEO_CALLBACKS: Once<VideoCallbacks> = Once::new();

static FRAMEBUFFER_INFO: AtomicPtr<FramebufferInfoC> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_video_callbacks(callbacks: VideoCallbacks) {
    let _ = VIDEO_CALLBACKS.call_once(|| callbacks);
}

pub fn draw_rect_filled_fast(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.draw_rect_filled_fast {
            return video_result_from_code(cb(x, y, w, h, color));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn draw_line(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.draw_line {
            return video_result_from_code(cb(x0, y0, x1, y1, color));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn draw_circle(cx: i32, cy: i32, radius: i32, color: u32) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.draw_circle {
            return video_result_from_code(cb(cx, cy, radius, color));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn draw_circle_filled(cx: i32, cy: i32, radius: i32, color: u32) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.draw_circle_filled {
            return video_result_from_code(cb(cx, cy, radius, color));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn font_draw_string(
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.font_draw_string {
            return cb(x, y, str_ptr, fg_color, bg_color);
        }
    }
    -1
}

pub fn framebuffer_blit(
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.framebuffer_blit {
            return video_result_from_code(cb(src_x, src_y, dst_x, dst_y, width, height));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn framebuffer_get_info() -> *mut FramebufferInfoC {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.framebuffer_get_info {
            let ptr = cb();
            FRAMEBUFFER_INFO.store(ptr, Ordering::Relaxed);
            return ptr;
        }
    }
    FRAMEBUFFER_INFO.load(Ordering::Relaxed)
}

pub fn roulette_draw(fate: u32) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.roulette_draw {
            return video_result_from_code(cb(fate));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn surface_draw_rect_filled_fast(
    task_id: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_draw_rect_filled_fast {
            return video_result_from_code(cb(task_id, x, y, w, h, color));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn surface_draw_line(
    task_id: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_draw_line {
            return video_result_from_code(cb(task_id, x0, y0, x1, y1, color));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn surface_draw_circle(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_draw_circle {
            return video_result_from_code(cb(task_id, cx, cy, radius, color));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn surface_draw_circle_filled(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_draw_circle_filled {
            return video_result_from_code(cb(task_id, cx, cy, radius, color));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn surface_font_draw_string(
    task_id: u32,
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_font_draw_string {
            return cb(task_id, x, y, str_ptr, fg_color, bg_color);
        }
    }
    -1
}

pub fn surface_blit(
    task_id: u32,
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_blit {
            return video_result_from_code(cb(task_id, src_x, src_y, dst_x, dst_y, width, height));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn compositor_present() -> Result<bool, VideoError> {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.compositor_present {
            let rc = cb();
            if rc < 0 {
                return Err(VideoError::Invalid);
            }
            return Ok(rc != 0);
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_enumerate_windows {
            return cb(out_buffer, max_count);
        }
    }
    0
}

pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_set_window_position {
            return cb(task_id, x, y);
        }
    }
    -1
}

pub fn surface_set_window_state(task_id: u32, state: u8) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_set_window_state {
            return cb(task_id, state);
        }
    }
    -1
}

pub fn surface_raise_window(task_id: u32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_raise_window {
            return cb(task_id);
        }
    }
    -1
}

fn video_result_from_code(rc: c_int) -> VideoResult {
    if rc == 0 {
        Ok(())
    } else {
        Err(VideoError::Invalid)
    }
}
