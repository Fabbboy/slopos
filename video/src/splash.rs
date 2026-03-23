use slopos_sync::IrqMutex;

use crate::framebuffer;
use crate::graphics::{GraphicsContext, GraphicsResult};
use crate::kernel_font;
use slopos_abi::draw::{Canvas, Color32};
use slopos_abi::video_traits::VideoError;
use slopos_gfx::canvas_ops;

/// Splash background: opaque black.  Previous value 0x00000000 was
/// transparent-black which broke anti-aliased text blending on the
/// write-only MMIO framebuffer (read-back returns 0 → dark fringe).
const SPLASH_BG_COLOR: Color32 = Color32(0xFF00_0000);
const SPLASH_TEXT_COLOR: Color32 = Color32(0xE6E6_E6FF);
const SPLASH_SUBTEXT_COLOR: Color32 = Color32(0x9A9A_9AFF);
const SPLASH_ACCENT_COLOR: Color32 = Color32(0x00C2_7FFF);
const SPLASH_PROGRESS_TRACK_COLOR: Color32 = Color32(0x1A1A_1AFF);
const SPLASH_PROGRESS_FRAME_COLOR: Color32 = Color32(0x2E2E_2EFF);

const SPLASH_PROGRESS_MIN_WIDTH: i32 = 220;
const SPLASH_PROGRESS_MAX_WIDTH: i32 = 360;
const SPLASH_PROGRESS_MIN_HEIGHT: i32 = 8;
const SPLASH_PROGRESS_MAX_HEIGHT: i32 = 12;
const SPLASH_MESSAGE_MIN_WIDTH: i32 = 240;
const SPLASH_MESSAGE_MAX_WIDTH: i32 = 420;
const SPLASH_MESSAGE_HEIGHT: i32 = 18;

/// Font size for splash screen text (proportional renderer).
const SPLASH_TITLE_SIZE: u16 = 22;
const SPLASH_SUBTITLE_SIZE: u16 = 14;
const SPLASH_MESSAGE_SIZE: u16 = 12;

const TEXT_TITLE: &str = "SLOPOS";
const TEXT_SUBTITLE: &str = "Safe boot";
const TEXT_INIT: &str = "Starting services...";

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

static STATE: IrqMutex<SplashState> = IrqMutex::new(SplashState::new());

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

fn measure_center_x(center_x: i32, text: &str, size_px: u16) -> i32 {
    let w = kernel_font::with_renderer(|r| r.measure_text(text, size_px).0).unwrap_or(0);
    center_x - w / 2
}

fn splash_layout(width: i32, height: i32) -> SplashLayout {
    let min_dim = width.min(height);
    let ring_radius = (min_dim / 20).clamp(18, 32);
    let center_x = width / 2;
    let ring_center_y = height / 2 - (min_dim / 8).clamp(30, 60);
    let title_y = ring_center_y + ring_radius + 12;
    let subtitle_y = title_y + 26;
    let message_y = subtitle_y + 22;
    let progress_w =
        (min_dim * 5 / 10).clamp(SPLASH_PROGRESS_MIN_WIDTH, SPLASH_PROGRESS_MAX_WIDTH);
    let progress_h =
        (min_dim / 120).clamp(SPLASH_PROGRESS_MIN_HEIGHT, SPLASH_PROGRESS_MAX_HEIGHT);
    let progress_x = center_x - (progress_w / 2);
    let progress_y = message_y + 22;
    let message_w =
        (min_dim * 55 / 100).clamp(SPLASH_MESSAGE_MIN_WIDTH, SPLASH_MESSAGE_MAX_WIDTH);
    let message_x = center_x - (message_w / 2);

    SplashLayout {
        center_x,
        ring_center_y,
        ring_radius,
        title_x: measure_center_x(center_x, TEXT_TITLE, SPLASH_TITLE_SIZE),
        title_y,
        subtitle_x: measure_center_x(center_x, TEXT_SUBTITLE, SPLASH_SUBTITLE_SIZE),
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
    framebuffer::snapshot().is_some()
}

fn ensure_framebuffer_ready() -> GraphicsResult<()> {
    if framebuffer_ready() {
        Ok(())
    } else {
        Err(VideoError::NoFramebuffer)
    }
}

fn splash_draw_logo(ctx: &mut GraphicsContext, center_x: i32, center_y: i32, ring_radius: i32) {
    canvas_ops::circle_filled(ctx, center_x, center_y, ring_radius, SPLASH_ACCENT_COLOR);
    canvas_ops::circle_filled(ctx, center_x, center_y, ring_radius - 4, SPLASH_BG_COLOR);
    canvas_ops::fill_rect(
        ctx,
        center_x - 40,
        center_y + ring_radius + 10,
        80,
        2,
        SPLASH_ACCENT_COLOR,
    );
}

fn splash_draw_progress_bar(
    ctx: &mut GraphicsContext,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    progress: i32,
) {
    canvas_ops::fill_rect(ctx, x, y, width, height, SPLASH_PROGRESS_TRACK_COLOR);
    canvas_ops::rect(
        ctx,
        x - 1,
        y - 1,
        width + 2,
        height + 2,
        SPLASH_PROGRESS_FRAME_COLOR,
    );

    if progress > 0 {
        let fill_width = (width * progress) / 100;
        canvas_ops::fill_rect(ctx, x, y, fill_width, height, SPLASH_ACCENT_COLOR);
    }
}

fn draw_text(ctx: &mut GraphicsContext, x: i32, y: i32, text: &str, size: u16, color: Color32) {
    kernel_font::with_renderer(|r| {
        r.draw_text(ctx, x, y, text, size, color, SPLASH_BG_COLOR);
    });
}

pub fn splash_show_boot_screen() -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;
    let mut ctx = GraphicsContext::new()?;

    let mut state = STATE.lock();
    let bg_px = ctx.pixel_format().encode(SPLASH_BG_COLOR);
    ctx.clear_canvas(bg_px);

    let width = ctx.width() as i32;
    let height = ctx.height() as i32;
    let layout = splash_layout(width, height);

    splash_draw_logo(
        &mut ctx,
        layout.center_x,
        layout.ring_center_y,
        layout.ring_radius,
    );

    draw_text(
        &mut ctx,
        layout.title_x,
        layout.title_y,
        TEXT_TITLE,
        SPLASH_TITLE_SIZE,
        SPLASH_TEXT_COLOR,
    );
    draw_text(
        &mut ctx,
        layout.subtitle_x,
        layout.subtitle_y,
        TEXT_SUBTITLE,
        SPLASH_SUBTITLE_SIZE,
        SPLASH_SUBTEXT_COLOR,
    );
    draw_text(
        &mut ctx,
        layout.message_x,
        layout.message_y,
        TEXT_INIT,
        SPLASH_MESSAGE_SIZE,
        SPLASH_SUBTEXT_COLOR,
    );

    splash_draw_progress_bar(
        &mut ctx,
        layout.progress_x,
        layout.progress_y,
        layout.progress_w,
        layout.progress_h,
        0,
    );

    state.active = true;
    state.progress = 0;
    Ok(())
}

pub fn splash_update_progress(progress: i32, message: &[u8]) -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;

    let mut ctx = GraphicsContext::new()?;
    let width = ctx.width() as i32;
    let height = ctx.height() as i32;
    let layout = splash_layout(width, height);

    canvas_ops::fill_rect(
        &mut ctx,
        layout.message_x,
        layout.message_y,
        layout.message_w,
        SPLASH_MESSAGE_HEIGHT,
        SPLASH_BG_COLOR,
    );

    if !message.is_empty() {
        // Convert &[u8] to &str (strip null terminator).
        let end = message.iter().position(|&b| b == 0).unwrap_or(message.len());
        if let Ok(s) = core::str::from_utf8(&message[..end]) {
            draw_text(
                &mut ctx,
                layout.message_x,
                layout.message_y,
                s,
                SPLASH_MESSAGE_SIZE,
                SPLASH_SUBTEXT_COLOR,
            );
        }
    }

    splash_draw_progress_bar(
        &mut ctx,
        layout.progress_x,
        layout.progress_y,
        layout.progress_w,
        layout.progress_h,
        progress,
    );
    Ok(())
}

pub fn splash_report_progress(progress: i32, message: &[u8]) -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;

    let mut state = STATE.lock();
    if !state.active {
        return Err(VideoError::Invalid);
    }

    state.progress = progress.min(100);
    splash_update_progress(state.progress, message)?;
    Ok(())
}

pub fn splash_finish() -> GraphicsResult<()> {
    let mut state = STATE.lock();
    if state.active {
        splash_report_progress(100, b"Boot complete\0")?;
        state.active = false;
    }
    Ok(())
}

pub fn splash_clear() -> GraphicsResult<()> {
    ensure_framebuffer_ready()?;
    let mut ctx = GraphicsContext::new()?;
    let bg_px = ctx.pixel_format().encode(SPLASH_BG_COLOR);
    ctx.clear_canvas(bg_px);
    Ok(())
}
