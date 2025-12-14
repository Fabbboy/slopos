#![allow(dead_code)]

use core::ffi::c_char;

use spin::Mutex;

use crate::font::font_draw_string;
use crate::framebuffer;
use crate::graphics;
use slopos_drivers::pit;

const SPLASH_BG_COLOR: u32 = 0x0011_22FF;
const SPLASH_LOGO_COLOR: u32 = 0xFFFF_FFFF;
const SPLASH_TEXT_COLOR: u32 = 0xFFFF_FFFF;
const SPLASH_PROGRESS_COLOR: u32 = 0x00FF_88FF;

const SPLASH_PROGRESS_WIDTH: i32 = 300;
const SPLASH_PROGRESS_HEIGHT: i32 = 20;

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

fn splash_draw_logo(center_x: i32, center_y: i32) -> i32 {
    if !framebuffer_ready() {
        return -1;
    }

    let logo_width = 300;
    let logo_height = 150;
    let logo_x = center_x - logo_width / 2;
    let logo_y = center_y - logo_height / 2;

    for y in 0..logo_height {
        let gradient_intensity = 0x40 + (y * 0x80 / logo_height);
        let gradient_color = ((gradient_intensity as u32) << 24) | ((gradient_intensity as u32) << 16) | 0xFF;
        graphics::graphics_draw_hline(logo_x, logo_x + logo_width, logo_y + y, gradient_color);
    }

    graphics::graphics_draw_rect(logo_x - 2, logo_y - 2, logo_width + 4, logo_height + 4, SPLASH_LOGO_COLOR);

    let letter_spacing = 60;
    let mut letter_start_x = logo_x + 30;
    let letter_y = logo_y + 40;
    let letter_height = 70;

    // S
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y, 40, 15, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y + 25, 40, 15, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y + 55, 40, 15, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y, 15, 40, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x + 25, letter_y + 30, 15, 40, SPLASH_LOGO_COLOR);

    // L
    letter_start_x += letter_spacing;
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y, 15, letter_height, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y + letter_height - 15, 40, 15, SPLASH_LOGO_COLOR);

    // O
    letter_start_x += letter_spacing;
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y, 40, 15, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y + letter_height - 15, 40, 15, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y, 15, letter_height, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x + 25, letter_y, 15, letter_height, SPLASH_LOGO_COLOR);

    // P
    letter_start_x += letter_spacing;
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y, 15, letter_height, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y, 40, 15, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x, letter_y + 25, 40, 15, SPLASH_LOGO_COLOR);
    graphics::graphics_draw_rect_filled(letter_start_x + 25, letter_y, 15, 40, SPLASH_LOGO_COLOR);

    0
}

fn splash_draw_progress_bar(x: i32, y: i32, width: i32, height: i32, progress: i32) -> i32 {
    if !framebuffer_ready() {
        return -1;
    }

    graphics::graphics_draw_rect_filled(x, y, width, height, 0x3333_33FF);
    graphics::graphics_draw_rect(x - 1, y - 1, width + 2, height + 2, SPLASH_LOGO_COLOR);

    if progress > 0 {
        let fill_width = (width * progress) / 100;
        graphics::graphics_draw_rect_filled(x, y, fill_width, height, SPLASH_PROGRESS_COLOR);
    }

    0
}

#[unsafe(no_mangle)]
pub fn splash_show_boot_screen() -> i32 {
    if !framebuffer_ready() {
        return -1;
    }

    let mut state = STATE.lock();
    framebuffer::framebuffer_clear(SPLASH_BG_COLOR);

    let width = framebuffer::framebuffer_get_width() as i32;
    let height = framebuffer::framebuffer_get_height() as i32;
    let center_x = width / 2;
    let center_y = height / 2;

    splash_draw_logo(center_x, center_y - 80);
    font_draw_string(center_x - 80, center_y + 100, b"SlopOS v0.000069\0".as_ptr() as *const c_char, SPLASH_TEXT_COLOR, 0);
    font_draw_string(
        center_x - 120,
        center_y + 120,
        b"the ultimate vibe slop experience\0".as_ptr() as *const c_char,
        SPLASH_TEXT_COLOR,
        0,
    );
    font_draw_string(center_x - 40, center_y + 160, b"Initializing...\0".as_ptr() as *const c_char, SPLASH_TEXT_COLOR, 0);

    let progress_bar_x = center_x - SPLASH_PROGRESS_WIDTH / 2;
    let progress_bar_y = center_y + 200;
    splash_draw_progress_bar(progress_bar_x, progress_bar_y, SPLASH_PROGRESS_WIDTH, SPLASH_PROGRESS_HEIGHT, 0);

    state.active = true;
    state.progress = 0;
    0
}

#[unsafe(no_mangle)]
pub fn splash_update_progress(progress: i32, message: *const c_char) -> i32 {
    if !framebuffer_ready() {
        return -1;
    }

    let width = framebuffer::framebuffer_get_width() as i32;
    let height = framebuffer::framebuffer_get_height() as i32;
    let center_x = width / 2;
    let center_y = height / 2;

    graphics::graphics_draw_rect_filled(center_x - 150, center_y + 155, 300, 20, SPLASH_BG_COLOR);
    if !message.is_null() {
        font_draw_string(center_x - 70, center_y + 160, message, SPLASH_TEXT_COLOR, 0);
    }

    let progress_bar_x = center_x - SPLASH_PROGRESS_WIDTH / 2;
    let progress_bar_y = center_y + 200;
    splash_draw_progress_bar(
        progress_bar_x,
        progress_bar_y,
        SPLASH_PROGRESS_WIDTH,
        SPLASH_PROGRESS_HEIGHT,
        progress,
    );
    0
}

#[unsafe(no_mangle)]
pub fn splash_report_progress(progress: i32, message: *const c_char) -> i32 {
    if !framebuffer_ready() {
        return -1;
    }

    let mut state = STATE.lock();
    if !state.active {
        return -1;
    }

    state.progress = progress.min(100);
    splash_update_progress(state.progress, message);

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
    0
}

#[unsafe(no_mangle)]
pub fn splash_finish() -> i32 {
    let mut state = STATE.lock();
    if state.active {
        splash_report_progress(100, b"Boot complete\0".as_ptr() as *const c_char);
        pit::pit_poll_delay_ms(250);
        state.active = false;
    }
    0
}

#[unsafe(no_mangle)]
pub fn splash_clear() -> i32 {
    if !framebuffer_ready() {
        return -1;
    }
    framebuffer::framebuffer_clear(0);
    0
}


