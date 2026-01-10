use core::ffi::c_char;

use spin::Mutex;

use crate::font;
use crate::framebuffer;
use crate::graphics::{self, GraphicsContext, GraphicsResult};
use slopos_drivers::pit;
use slopos_drivers::video_bridge::VideoError;

const SPLASH_BG_COLOR: u32 = 0x0000_0000;
const SPLASH_TEXT_COLOR: u32 = 0xE6E6_E6FF;
const SPLASH_SUBTEXT_COLOR: u32 = 0x9A9A_9AFF;
const SPLASH_ACCENT_COLOR: u32 = 0x00C2_7FFF;
const SPLASH_PROGRESS_TRACK_COLOR: u32 = 0x1A1A_1AFF;
const SPLASH_PROGRESS_FRAME_COLOR: u32 = 0x2E2E_2EFF;

const SPLASH_PROGRESS_MIN_WIDTH: i32 = 220;
const SPLASH_PROGRESS_MAX_WIDTH: i32 = 360;
const SPLASH_PROGRESS_MIN_HEIGHT: i32 = 8;
const SPLASH_PROGRESS_MAX_HEIGHT: i32 = 12;
const SPLASH_MESSAGE_MIN_WIDTH: i32 = 240;
const SPLASH_MESSAGE_MAX_WIDTH: i32 = 420;
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

struct SplashLayout {
    center_x: i32,
    ring_center_y: i32,
    ring_radius: i32,
    title_x: i32,
    title_y: i32,
    subtitle_x: i32,
    subtitle_y: i32,
    message_x: i32,
    message_y: i32,
    message_w: i32,
    progress_x: i32,
    progress_y: i32,
    progress_w: i32,
    progress_h: i32,
}

fn text_center_x(center_x: i32, text: &[u8]) -> i32 {
    let chars = text.len().saturating_sub(1) as i32;
    center_x - (chars * 8 / 2)
}

fn splash_layout(width: i32, height: i32) -> SplashLayout {
    let min_dim = width.min(height);
    let ring_radius = (min_dim / 20).clamp(18, 32);
    let center_x = width / 2;
    let ring_center_y = height / 2 - (min_dim / 8).clamp(30, 60);
    let title_y = ring_center_y + ring_radius + 12;
    let subtitle_y = title_y + 18;
    let message_y = subtitle_y + 22;
    let progress_w = (min_dim * 5 / 10).clamp(SPLASH_PROGRESS_MIN_WIDTH, SPLASH_PROGRESS_MAX_WIDTH);
    let progress_h = (min_dim / 120).clamp(SPLASH_PROGRESS_MIN_HEIGHT, SPLASH_PROGRESS_MAX_HEIGHT);
    let progress_x = center_x - (progress_w / 2);
    let progress_y = message_y + 22;
    let message_w = (min_dim * 55 / 100).clamp(SPLASH_MESSAGE_MIN_WIDTH, SPLASH_MESSAGE_MAX_WIDTH);
    let message_x = center_x - (message_w / 2);

    SplashLayout {
        center_x,
        ring_center_y,
        ring_radius,
        title_x: text_center_x(center_x, TEXT_TITLE),
        title_y,
        subtitle_x: text_center_x(center_x, TEXT_SUBTITLE),
        subtitle_y,
        message_x,
        message_y,
        message_w,
        progress_x,
        progress_y,
        progress_w,
        progress_h,
    }
}

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

fn splash_draw_logo(
    ctx: &GraphicsContext,
    center_x: i32,
    center_y: i32,
    ring_radius: i32,
) -> GraphicsResult<()> {
    graphics::graphics_draw_circle_filled_ctx(
        ctx,
        center_x,
        center_y,
        ring_radius,
        SPLASH_ACCENT_COLOR,
    )?;
    graphics::graphics_draw_circle_filled_ctx(
        ctx,
        center_x,
        center_y,
        ring_radius - 4,
        SPLASH_BG_COLOR,
    )?;
    graphics::graphics_draw_rect_filled_fast_ctx(
        ctx,
        center_x - 40,
        center_y + ring_radius + 10,
        80,
        2,
        SPLASH_ACCENT_COLOR,
    )?;

    Ok(())
}

fn splash_draw_progress_bar(
    ctx: &GraphicsContext,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    progress: i32,
) -> GraphicsResult<()> {
    graphics::graphics_draw_rect_filled_fast_ctx(
        ctx,
        x,
        y,
        width,
        height,
        SPLASH_PROGRESS_TRACK_COLOR,
    )?;
    graphics::graphics_draw_rect_ctx(
        ctx,
        x - 1,
        y - 1,
        width + 2,
        height + 2,
        SPLASH_PROGRESS_FRAME_COLOR,
    )?;

    if progress > 0 {
        let fill_width = (width * progress) / 100;
        graphics::graphics_draw_rect_filled_fast_ctx(
            ctx,
            x,
            y,
            fill_width,
            height,
            SPLASH_ACCENT_COLOR,
        )?;
    }

    Ok(())
}
pub fn splash_show_boot_screen() -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;
    let ctx = GraphicsContext::new()?;

    let mut state = STATE.lock();
    framebuffer::framebuffer_clear(SPLASH_BG_COLOR);

    let width = ctx.width() as i32;
    let height = ctx.height() as i32;
    let layout = splash_layout(width, height);

    splash_draw_logo(
        &ctx,
        layout.center_x,
        layout.ring_center_y,
        layout.ring_radius,
    )?;
    if font::font_draw_string_ctx(
        &ctx,
        layout.title_x,
        layout.title_y,
        TEXT_TITLE.as_ptr() as *const c_char,
        SPLASH_TEXT_COLOR,
        0,
    ) != 0
    {
        return Err(VideoError::Invalid);
    }
    if font::font_draw_string_ctx(
        &ctx,
        layout.subtitle_x,
        layout.subtitle_y,
        TEXT_SUBTITLE.as_ptr() as *const c_char,
        SPLASH_SUBTEXT_COLOR,
        0,
    ) != 0
    {
        return Err(VideoError::Invalid);
    }
    if font::font_draw_string_ctx(
        &ctx,
        layout.message_x,
        layout.message_y,
        TEXT_INIT.as_ptr() as *const c_char,
        SPLASH_SUBTEXT_COLOR,
        0,
    ) != 0
    {
        return Err(VideoError::Invalid);
    }

    splash_draw_progress_bar(
        &ctx,
        layout.progress_x,
        layout.progress_y,
        layout.progress_w,
        layout.progress_h,
        0,
    )?;

    state.active = true;
    state.progress = 0;
    Ok(())
}
pub fn splash_update_progress(progress: i32, message: *const c_char) -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;

    let ctx = GraphicsContext::new()?;
    let width = ctx.width() as i32;
    let height = ctx.height() as i32;
    let layout = splash_layout(width, height);

    graphics::graphics_draw_rect_filled_fast_ctx(
        &ctx,
        layout.message_x,
        layout.message_y,
        layout.message_w,
        SPLASH_MESSAGE_HEIGHT,
        SPLASH_BG_COLOR,
    )?;
    if !message.is_null()
        && font::font_draw_string_ctx(
            &ctx,
            layout.message_x,
            layout.message_y,
            message,
            SPLASH_SUBTEXT_COLOR,
            0,
        ) != 0
    {
        return Err(VideoError::Invalid);
    }

    splash_draw_progress_bar(
        &ctx,
        layout.progress_x,
        layout.progress_y,
        layout.progress_w,
        layout.progress_h,
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
