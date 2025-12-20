use core::ffi::c_char;

use spin::Mutex;

use crate::font::font_draw_string;
use crate::framebuffer;
use crate::graphics::{self, GraphicsResult};
use slopos_drivers::pit;
use slopos_drivers::video_bridge::VideoError;

const SPLASH_BG_COLOR: u32 = 0x0000_0000;
const SPLASH_TEXT_COLOR: u32 = 0xE6E6_E6FF;
const SPLASH_SUBTEXT_COLOR: u32 = 0x9A9A_9AFF;
const SPLASH_ACCENT_COLOR: u32 = 0x00C2_7FFF;
const SPLASH_PROGRESS_TRACK_COLOR: u32 = 0x1A1A_1AFF;
const SPLASH_PROGRESS_FRAME_COLOR: u32 = 0x2E2E_2EFF;

const SPLASH_PROGRESS_WIDTH: i32 = 260;
const SPLASH_PROGRESS_HEIGHT: i32 = 10;
const SPLASH_MESSAGE_WIDTH: i32 = 320;
const SPLASH_MESSAGE_HEIGHT: i32 = 18;

const TEXT_TITLE: &[u8] = b"SLOPOS\0";
const TEXT_SUBTITLE: &[u8] = b"Safe boot\0";
const TEXT_INIT: &[u8] = b"Starting services...\0";

struct SplashState {
    active: bool,
    progress: i32,
}

impl SplashState {
    const fn new() -> Self {
        Self {
            active: false,
            progress: 0,
        }
    }
}

static STATE: Mutex<SplashState> = Mutex::new(SplashState::new());

fn framebuffer_ready() -> bool {
    framebuffer::framebuffer_is_initialized() != 0
}

fn ensure_framebuffer_ready() -> GraphicsResult<()> {
    if framebuffer_ready() {
        Ok(())
    } else {
        Err(VideoError::NoFramebuffer)
    }
}

fn splash_draw_logo(center_x: i32, center_y: i32) -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;

    let ring_radius = 28;
    graphics::graphics_draw_circle_filled(center_x, center_y, ring_radius, SPLASH_ACCENT_COLOR)?;
    graphics::graphics_draw_circle_filled(
        center_x,
        center_y,
        ring_radius - 4,
        SPLASH_BG_COLOR,
    )?;
    graphics::graphics_draw_rect_filled(
        center_x - 40,
        center_y + ring_radius + 10,
        80,
        2,
        SPLASH_ACCENT_COLOR,
    )?;

    Ok(())
}

fn splash_draw_progress_bar(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    progress: i32,
) -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;

    graphics::graphics_draw_rect_filled(x, y, width, height, SPLASH_PROGRESS_TRACK_COLOR)?;
    graphics::graphics_draw_rect(
        x - 1,
        y - 1,
        width + 2,
        height + 2,
        SPLASH_PROGRESS_FRAME_COLOR,
    )?;

    if progress > 0 {
        let fill_width = (width * progress) / 100;
        graphics::graphics_draw_rect_filled(x, y, fill_width, height, SPLASH_ACCENT_COLOR)?;
    }

    Ok(())
}
pub fn splash_show_boot_screen() -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;

    let mut state = STATE.lock();
    framebuffer::framebuffer_clear(SPLASH_BG_COLOR);

    let width = framebuffer::framebuffer_get_width() as i32;
    let height = framebuffer::framebuffer_get_height() as i32;
    let center_x = width / 2;
    let center_y = height / 2;

    splash_draw_logo(center_x, center_y - 90)?;
    let title_x = center_x - ((TEXT_TITLE.len() as i32 - 1) * 8 / 2);
    if font_draw_string(
        title_x,
        center_y - 35,
        TEXT_TITLE.as_ptr() as *const c_char,
        SPLASH_TEXT_COLOR,
        0,
    ) != 0
    {
        return Err(VideoError::Invalid);
    }
    let subtitle_x = center_x - ((TEXT_SUBTITLE.len() as i32 - 1) * 8 / 2);
    if font_draw_string(
        subtitle_x,
        center_y - 15,
        TEXT_SUBTITLE.as_ptr() as *const c_char,
        SPLASH_SUBTEXT_COLOR,
        0,
    ) != 0
    {
        return Err(VideoError::Invalid);
    }
    let message_x = center_x - (SPLASH_MESSAGE_WIDTH / 2);
    if font_draw_string(
        message_x,
        center_y + 15,
        TEXT_INIT.as_ptr() as *const c_char,
        SPLASH_SUBTEXT_COLOR,
        0,
    ) != 0
    {
        return Err(VideoError::Invalid);
    }

    let progress_bar_x = center_x - SPLASH_PROGRESS_WIDTH / 2;
    let progress_bar_y = center_y + 40;
    splash_draw_progress_bar(
        progress_bar_x,
        progress_bar_y,
        SPLASH_PROGRESS_WIDTH,
        SPLASH_PROGRESS_HEIGHT,
        0,
    )?;

    state.active = true;
    state.progress = 0;
    Ok(())
}
pub fn splash_update_progress(progress: i32, message: *const c_char) -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;

    let width = framebuffer::framebuffer_get_width() as i32;
    let height = framebuffer::framebuffer_get_height() as i32;
    let center_x = width / 2;
    let center_y = height / 2;

    graphics::graphics_draw_rect_filled(
        center_x - (SPLASH_MESSAGE_WIDTH / 2),
        center_y + 15,
        SPLASH_MESSAGE_WIDTH,
        SPLASH_MESSAGE_HEIGHT,
        SPLASH_BG_COLOR,
    )?;
    if !message.is_null()
        && font_draw_string(
            center_x - (SPLASH_MESSAGE_WIDTH / 2),
            center_y + 15,
            message,
            SPLASH_SUBTEXT_COLOR,
            0,
        ) != 0
    {
        return Err(VideoError::Invalid);
    }

    let progress_bar_x = center_x - SPLASH_PROGRESS_WIDTH / 2;
    let progress_bar_y = center_y + 40;
    splash_draw_progress_bar(
        progress_bar_x,
        progress_bar_y,
        SPLASH_PROGRESS_WIDTH,
        SPLASH_PROGRESS_HEIGHT,
        progress,
    )?;
    Ok(())
}
pub fn splash_report_progress(progress: i32, message: *const c_char) -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;

    let mut state = STATE.lock();
    if !state.active {
        return Err(VideoError::Invalid);
    }

    state.progress = progress.min(100);
    splash_update_progress(state.progress, message)?;

    let delay_ms = if state.progress <= 20 {
        300
    } else if state.progress <= 40 {
        250
    } else if state.progress <= 60 {
        280
    } else if state.progress <= 80 {
        320
    } else if state.progress <= 95 {
        280
    } else {
        250
    };

    pit::pit_poll_delay_ms(delay_ms as u32);
    Ok(())
}
pub fn splash_finish() -> GraphicsResult<()> {
    let mut state = STATE.lock();
    if state.active {
        splash_report_progress(100, b"Boot complete\0".as_ptr() as *const c_char)?;
        pit::pit_poll_delay_ms(250);
        state.active = false;
    }
    Ok(())
}
pub fn splash_clear() -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;
    framebuffer::framebuffer_clear(SPLASH_BG_COLOR);
    Ok(())
}
