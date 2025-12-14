#![allow(dead_code)]

use core::ffi::c_void;

const ROULETTE_BLANK_COLOR: u32 = 0x1818_18FF;
const ROULETTE_BLANK_HIGHLIGHT: u32 = 0x4444_44FF;
const ROULETTE_COLORED_HIGHLIGHT: u32 = 0x66FF_66FF;
const ROULETTE_POINTER_COLOR: u32 = 0xFFFF_00FF;

const ROULETTE_SEGMENT_COUNT: i32 = 12;
const ROULETTE_TRIG_SCALE: i32 = 1024;
const ROULETTE_WHEEL_RADIUS: i32 = 120;
const ROULETTE_INNER_RADIUS: i32 = 36;
const ROULETTE_POINTER_WIDTH: i32 = 18;
const ROULETTE_DEGREE_STEPS: i32 = 360;
const ROULETTE_SEGMENT_DEGREES: i32 = 360 / ROULETTE_SEGMENT_COUNT;
const ROULETTE_SPIN_LOOPS: i32 = 4;
const ROULETTE_SPIN_DURATION_MS: i32 = 3600;
const ROULETTE_SPIN_FRAME_DELAY_MS: i32 = 12;

// Public colors pulled from the legacy header.
pub const ROULETTE_BG_COLOR: u32 = 0x0000_00FF;
pub const ROULETTE_WHEEL_COLOR: u32 = 0xFFD7_00FF;
pub const ROULETTE_TEXT_COLOR: u32 = 0xFFFF_FFFF;
pub const ROULETTE_WIN_COLOR: u32 = 0x00FF_00FF;
pub const ROULETTE_LOSE_COLOR: u32 = 0xFF00_00FF;
pub const ROULETTE_EVEN_COLOR: u32 = 0xFF44_44FF;
pub const ROULETTE_ODD_COLOR: u32 = 0x44FF_44FF;
pub const ROULETTE_RESULT_DELAY_MS: u32 = 5000;

#[repr(C)]
pub struct RouletteBackend {
    pub ctx: *mut c_void,
    pub get_size: Option<extern "C" fn(*mut c_void, *mut i32, *mut i32) -> i32>,
    pub fill_rect: Option<extern "C" fn(*mut c_void, i32, i32, i32, i32, u32) -> i32>,
    pub draw_line: Option<extern "C" fn(*mut c_void, i32, i32, i32, i32, u32) -> i32>,
    pub draw_circle: Option<extern "C" fn(*mut c_void, i32, i32, i32, u32) -> i32>,
    pub draw_circle_filled: Option<extern "C" fn(*mut c_void, i32, i32, i32, u32) -> i32>,
    pub draw_text: Option<extern "C" fn(*mut c_void, i32, i32, *const u8, u32, u32) -> i32>,
    pub sleep_ms: Option<extern "C" fn(*mut c_void, u32)>,
}

#[derive(Copy, Clone)]
struct RouletteSegment {
    is_colored: bool,
}

const SEGMENTS: [RouletteSegment; ROULETTE_SEGMENT_COUNT as usize] = [
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
];

const TEXT_UNKNOWN: &[u8] = b"? ? ?\0";
const TEXT_WIN: &[u8] = b"W I N !\0";
const TEXT_WIN_SUB: &[u8] = b"Fortune smiles upon the slop!\0";
const TEXT_LOSE: &[u8] = b"L O S E\0";
const TEXT_LOSE_SUB: &[u8] = b"L bozzo lol - try again!\0";
const TEXT_DEMO_TITLE: &[u8] = b"*** SLOPOS GRAPHICS SYSTEM OPERATIONAL ***\0";
const TEXT_DEMO_FB: &[u8] = b"Framebuffer: WORKING | Resolution: 1024x768\0";
const TEXT_DEMO_STATUS: &[u8] = b"Memory: OK | Graphics: OK | Text: OK\0";
const TEXT_HANDOFF: &[u8] = b"Shell launching... enjoy the demo\0";
const TEXT_WHEEL_TITLE: &[u8] = b"=== THE WHEEL OF FATE ===\0";
const TEXT_WHEEL_SUB: &[u8] = b"Pointers choose your destiny...\0";
const TEXT_CURRENCY_WIN: &[u8] = b"+10 W's (currency units)\0";
const TEXT_CURRENCY_LOSE: &[u8] = b"-10 W's (currency units)\0";
const TEXT_RESET: &[u8] = b"Press RESET to try again...\0";
const TEXT_CONTINUE: &[u8] = b"Continuing to OS...\0";

// Precomputed trig tables (scaled by 1024) carried over from the original C.
const COS_TABLE: [i16; (ROULETTE_SEGMENT_COUNT + 1) as usize] = [1024, 887, 512, 0, -512, -887, -1024, -887, -512, 0, 512, 887, 1024];
const SIN_TABLE: [i16; (ROULETTE_SEGMENT_COUNT + 1) as usize] = [0, 512, 887, 1024, 887, 512, 0, -512, -887, -1024, -887, -512, 0];

const COS360: [i16; ROULETTE_DEGREE_STEPS as usize] = [0; ROULETTE_DEGREE_STEPS as usize];

#[allow(clippy::unreadable_literal)]
const SIN360: [i16; ROULETTE_DEGREE_STEPS as usize] = [0; ROULETTE_DEGREE_STEPS as usize];

fn normalize_angle(degrees: i32) -> i32 {
    let mut angle = degrees % ROULETTE_DEGREE_STEPS;
    if angle < 0 {
        angle += ROULETTE_DEGREE_STEPS;
    }
    angle
}

fn cos_deg(degrees: i32) -> i16 {
    COS360[normalize_angle(degrees) as usize]
}

fn sin_deg(degrees: i32) -> i16 {
    SIN360[normalize_angle(degrees) as usize]
}

fn scale(value: i16, radius: i32) -> i32 {
    (value as i32 * radius) / ROULETTE_TRIG_SCALE
}

unsafe fn backend_get_size(b: &RouletteBackend, w: &mut i32, h: &mut i32) -> i32 {
    match b.get_size {
        Some(f) => f(b.ctx, w as *mut i32, h as *mut i32),
        None => -1,
    }
}

unsafe fn backend_fill_rect(b: &RouletteBackend, x: i32, y: i32, w: i32, h: i32, color: u32) -> i32 {
    match b.fill_rect {
        Some(f) => f(b.ctx, x, y, w, h, color),
        None => -1,
    }
}

unsafe fn backend_draw_line(b: &RouletteBackend, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) -> i32 {
    match b.draw_line {
        Some(f) => f(b.ctx, x0, y0, x1, y1, color),
        None => -1,
    }
}

unsafe fn backend_draw_circle(b: &RouletteBackend, cx: i32, cy: i32, radius: i32, color: u32) -> i32 {
    match b.draw_circle {
        Some(f) => f(b.ctx, cx, cy, radius, color),
        None => -1,
    }
}

unsafe fn backend_draw_circle_filled(b: &RouletteBackend, cx: i32, cy: i32, radius: i32, color: u32) -> i32 {
    match b.draw_circle_filled {
        Some(f) => f(b.ctx, cx, cy, radius, color),
        None => -1,
    }
}

unsafe fn backend_draw_text(b: &RouletteBackend, x: i32, y: i32, text: &[u8], fg: u32, bg: u32) -> i32 {
    match b.draw_text {
        Some(f) => f(b.ctx, x, y, text.as_ptr(), fg, bg),
        None => -1,
    }
}

unsafe fn backend_sleep_ms(b: &RouletteBackend, ms: u32) {
    if let Some(f) = b.sleep_ms {
        f(b.ctx, ms);
    }
}

fn segment_center_angle(segment_index: i32) -> i32 {
    segment_index * ROULETTE_SEGMENT_DEGREES + (ROULETTE_SEGMENT_DEGREES / 2)
}

fn draw_segment_wedge(b: &RouletteBackend, cx: i32, cy: i32, start_idx: usize, radius: i32, color: u32) {
    let inner = ROULETTE_INNER_RADIUS;
    let start_cos = COS_TABLE[start_idx];
    let start_sin = SIN_TABLE[start_idx];
    let end_cos = COS_TABLE[start_idx + 1];
    let end_sin = SIN_TABLE[start_idx + 1];

    for r in inner..=radius {
        let x1 = cx + scale(start_cos, r);
        let y1 = cy + scale(start_sin, r);
        let x2 = cx + scale(end_cos, r);
        let y2 = cy + scale(end_sin, r);
        unsafe {
            backend_draw_line(b, x1, y1, x2, y2, color);
        }
    }
}

fn draw_segment_divider(b: &RouletteBackend, cx: i32, cy: i32, idx: usize, radius: i32) {
    let x_outer = cx + scale(COS_TABLE[idx], radius + 2);
    let y_outer = cy + scale(SIN_TABLE[idx], radius + 2);
    unsafe {
        backend_draw_line(b, cx, cy, x_outer, y_outer, ROULETTE_WHEEL_COLOR);
    }
}

fn draw_roulette_wheel(b: &RouletteBackend, cx: i32, cy: i32, radius: i32, highlight_segment: i32) {
    unsafe {
        backend_draw_circle_filled(b, cx, cy, radius + 8, 0x0000_00FF);
        backend_draw_circle(b, cx, cy, radius + 8, ROULETTE_WHEEL_COLOR);
    }

    for i in 0..ROULETTE_SEGMENT_COUNT {
        let is_colored = SEGMENTS[i as usize].is_colored;
        let mut base_color = if is_colored { ROULETTE_ODD_COLOR } else { ROULETTE_BLANK_COLOR };
        if i == highlight_segment {
            base_color = if is_colored { ROULETTE_COLORED_HIGHLIGHT } else { ROULETTE_BLANK_HIGHLIGHT };
        }
        draw_segment_wedge(b, cx, cy, i as usize, radius, base_color);
        draw_segment_divider(b, cx, cy, i as usize, radius);
    }
    draw_segment_divider(b, cx, cy, ROULETTE_SEGMENT_COUNT as usize, radius);

    unsafe {
        backend_draw_circle_filled(b, cx, cy, ROULETTE_INNER_RADIUS + 6, ROULETTE_WHEEL_COLOR);
        backend_draw_circle_filled(b, cx, cy, ROULETTE_INNER_RADIUS, 0x0000_00FF);
    }
}

fn draw_pointer_for_angle(b: &RouletteBackend, cx: i32, cy: i32, radius: i32, angle_deg: i32, color: u32) {
    let dir_x = cos_deg(angle_deg);
    let dir_y = sin_deg(angle_deg);
    let perp_x = -dir_y;
    let perp_y = dir_x;

    let tip_radius = radius + 36;
    let base_radius = radius - 6;

    let tip_x = cx + scale(dir_x, tip_radius);
    let tip_y = cy + scale(dir_y, tip_radius);
    let base_x = cx + scale(dir_x, base_radius);
    let base_y = cy + scale(dir_y, base_radius);

    let offset_x = scale(perp_x, ROULETTE_POINTER_WIDTH);
    let offset_y = scale(perp_y, ROULETTE_POINTER_WIDTH);

    let left_x = base_x + offset_x;
    let left_y = base_y + offset_y;
    let right_x = base_x - offset_x;
    let right_y = base_y - offset_y;

    unsafe {
        backend_draw_line(b, tip_x, tip_y, left_x, left_y, color);
        backend_draw_line(b, tip_x, tip_y, right_x, right_y, color);
        backend_draw_line(b, left_x, left_y, right_x, right_y, color);
    }
}

fn draw_pointer_ticks(b: &RouletteBackend, cx: i32, cy: i32, radius: i32, angle_deg: i32, color: u32) {
    draw_pointer_for_angle(b, cx, cy, radius, angle_deg, color);
    draw_pointer_for_angle(b, cx, cy, radius, angle_deg + 180, color);
}

fn draw_fate_number(
    b: &RouletteBackend,
    cx: i32,
    y_pos: i32,
    fate_number: u32,
    revealed: bool,
) {
    if !revealed {
        unsafe {
            backend_fill_rect(b, cx - 100, y_pos, 200, 60, 0x3333_33FF);
            backend_draw_line(b, cx - 100, y_pos, cx + 100, y_pos, ROULETTE_WHEEL_COLOR);
            backend_draw_line(b, cx - 100, y_pos + 60, cx + 100, y_pos + 60, ROULETTE_WHEEL_COLOR);
            backend_draw_text(b, cx - 40, y_pos + 20, TEXT_UNKNOWN, ROULETTE_TEXT_COLOR, 0);
        }
        return;
    }

    let box_color = if fate_number & 1 == 1 { ROULETTE_ODD_COLOR } else { ROULETTE_EVEN_COLOR };
    unsafe {
        backend_fill_rect(b, cx - 100, y_pos, 200, 60, box_color);
        backend_draw_line(b, cx - 100, y_pos, cx + 100, y_pos, ROULETTE_WHEEL_COLOR);
        backend_draw_line(b, cx - 100, y_pos + 60, cx + 100, y_pos + 60, ROULETTE_WHEEL_COLOR);
    }

    let mut num_str = [0u8; 21];
    let mut len = 0usize;
    if fate_number == 0 {
        num_str[len] = b'0';
        len += 1;
    } else {
        let mut n = fate_number;
        let mut tmp = [0u8; 21];
        let mut t = 0usize;
        while n != 0 && t < tmp.len() {
            tmp[t] = b'0' + (n % 10) as u8;
            n /= 10;
            t += 1;
        }
        while t > 0 {
            len += 1;
            num_str[len - 1] = tmp[t - 1];
            t -= 1;
        }
    }
    let text_x = cx - (len as i32 * 8) / 2;
    unsafe {
        backend_draw_text(b, text_x, y_pos + 20, &num_str[..len], 0x0000_00FF, 0);
    }
}

fn draw_result_banner(b: &RouletteBackend, cx: i32, y_pos: i32, fate_number: u32) {
    let (result_text, sub_text, banner_color) = if fate_number & 1 == 1 {
        (TEXT_WIN, TEXT_WIN_SUB, ROULETTE_WIN_COLOR)
    } else {
        (TEXT_LOSE, TEXT_LOSE_SUB, ROULETTE_LOSE_COLOR)
    };

    unsafe {
        backend_fill_rect(b, cx - 200, y_pos, 400, 80, banner_color);
        backend_draw_line(b, cx - 202, y_pos - 2, cx + 202, y_pos - 2, ROULETTE_WHEEL_COLOR);
        backend_draw_line(b, cx - 202, y_pos + 82, cx + 202, y_pos + 82, ROULETTE_WHEEL_COLOR);
        backend_draw_text(b, cx - 60, y_pos + 15, result_text, 0x0000_00FF, 0);
        backend_draw_text(b, cx - 140, y_pos + 50, sub_text, 0x0000_00FF, 0);
    }
}

fn render_wheel_frame(
    b: &RouletteBackend,
    screen_width: i32,
    screen_height: i32,
    cx: i32,
    cy: i32,
    radius: i32,
    highlight_segment: i32,
    pointer_angle_deg: i32,
    last_pointer_angle: &mut i32,
    fate_number: u32,
    reveal_number: bool,
    clear_background: bool,
    draw_wheel: bool,
) {
    let region = radius + 80;
    let mut region_x = cx - region;
    let mut region_y = cy - region;
    let mut region_w = region * 2;
    let mut region_h = region * 2;

    if !clear_background && *last_pointer_angle >= 0 {
        draw_pointer_ticks(b, cx, cy, radius, *last_pointer_angle, ROULETTE_BG_COLOR);
    }

    if clear_background {
        if region_x < 0 {
            region_w += region_x;
            region_x = 0;
        }
        if region_y < 0 {
            region_h += region_y;
            region_y = 0;
        }
        if region_x + region_w > screen_width {
            region_w = screen_width - region_x;
        }
        if region_y + region_h > screen_height {
            region_h = screen_height - region_y;
        }
        unsafe {
            backend_fill_rect(b, region_x, region_y, region_w, region_h, ROULETTE_BG_COLOR);
        }
    }

    if draw_wheel {
        draw_roulette_wheel(b, cx, cy, radius, highlight_segment);
    }
    draw_pointer_ticks(b, cx, cy, radius, pointer_angle_deg, ROULETTE_POINTER_COLOR);
    draw_fate_number(b, cx, cy + radius + 30, fate_number, reveal_number);

    *last_pointer_angle = pointer_angle_deg;
}

fn segment_matches_parity(segment_index: i32, need_colored: bool) -> bool {
    let is_colored = SEGMENTS[(segment_index % ROULETTE_SEGMENT_COUNT) as usize].is_colored;
    if need_colored {
        is_colored
    } else {
        !is_colored
    }
}

fn choose_segment_for_parity(fate_number: u32, need_colored: bool) -> i32 {
    let start = (fate_number % ROULETTE_SEGMENT_COUNT as u32) as i32;
    for tries in 0..ROULETTE_SEGMENT_COUNT {
        let idx = (start + tries) % ROULETTE_SEGMENT_COUNT;
        if segment_matches_parity(idx, need_colored) {
            return idx;
        }
    }
    start
}

fn roulette_draw_demo_scene(b: &RouletteBackend, width: i32, height: i32) {
    unsafe {
        backend_fill_rect(b, 0, 0, width, height, 0x0011_22FF);
        backend_fill_rect(b, 20, 20, 300, 150, 0xFF00_00FF);
        backend_fill_rect(b, width - 320, 20, 300, 150, 0x00FF_00FF);
    }

    let cx = width / 2;
    let cy = height / 2;
    let mut radius = if width < height { width } else { height } / 8;
    if radius < 60 {
        radius = 60;
    }
    unsafe {
        backend_draw_circle(b, cx, cy, radius, 0xFFFF_00FF);
        backend_fill_rect(b, 0, 0, width, 4, 0xFFFF_FFFF);
        backend_fill_rect(b, 0, height - 4, width, 4, 0xFFFF_FFFF);
        backend_fill_rect(b, 0, 0, 4, height, 0xFFFF_FFFF);
        backend_fill_rect(b, width - 4, 0, 4, height, 0xFFFF_FFFF);
        backend_draw_text(b, 20, height - 140, TEXT_DEMO_TITLE, 0xFFFF_FFFF, 0);
        backend_draw_text(b, 20, height - 124, TEXT_DEMO_FB, 0xFFFF_FFFF, 0);
        backend_draw_text(b, 20, height - 108, TEXT_DEMO_STATUS, 0xFFFF_FFFF, 0);
    }
}

fn roulette_handoff_to_demo(b: &RouletteBackend, width: i32, height: i32) {
    unsafe {
        backend_fill_rect(b, 0, 0, width, height, ROULETTE_BG_COLOR);
        backend_draw_text(b, width / 2 - 140, height / 2 - 20, TEXT_HANDOFF, ROULETTE_TEXT_COLOR, 0);
        backend_sleep_ms(b, 400);
    }
    roulette_draw_demo_scene(b, width, height);
}

#[unsafe(no_mangle)]
pub extern "C" fn roulette_run(backend: *const RouletteBackend, fate_number: u32) -> i32 {
    if backend.is_null() {
        return -1;
    }
    let backend = unsafe { &*backend };

    let mut width = 0;
    let mut height = 0;
    if unsafe { backend_get_size(backend, &mut width, &mut height) } != 0 || width <= 0 || height <= 0 {
        return -1;
    }

    if unsafe { backend_fill_rect(backend, 0, 0, width, height, ROULETTE_BG_COLOR) } != 0 {
        return -1;
    }

    unsafe {
        backend_draw_text(backend, width / 2 - 150, 50, TEXT_WHEEL_TITLE, ROULETTE_WHEEL_COLOR, 0);
        backend_draw_text(backend, width / 2 - 120, 80, TEXT_WHEEL_SUB, ROULETTE_TEXT_COLOR, 0);
    }

    let mut radius = ROULETTE_WHEEL_RADIUS;
    let max_radius = (width.min(height) / 2) - 60;
    if radius > max_radius {
        radius = max_radius;
    }
    if radius < ROULETTE_INNER_RADIUS + 20 {
        radius = ROULETTE_INNER_RADIUS + 20;
    }

    let want_colored = (fate_number & 1) != 0;
    let mut start_segment = (fate_number % ROULETTE_SEGMENT_COUNT as u32) as i32;
    let target_segment = choose_segment_for_parity(fate_number, want_colored);
    if start_segment == target_segment {
        start_segment = (start_segment + 3) % ROULETTE_SEGMENT_COUNT;
    }

    unsafe {
        backend_sleep_ms(backend, 300);
    }

    let center_x = width / 2;
    let center_y = height / 2;
    let start_angle = segment_center_angle(start_segment);
    let target_angle = segment_center_angle(target_segment);
    let rotation_to_target = normalize_angle(target_angle - start_angle);
    let mut total_rotation = ROULETTE_SPIN_LOOPS * ROULETTE_DEGREE_STEPS + rotation_to_target;
    if total_rotation <= 0 {
        total_rotation += ROULETTE_DEGREE_STEPS;
    }

    let mut last_pointer_angle = -1;
    render_wheel_frame(
        backend,
        width,
        height,
        center_x,
        center_y,
        radius,
        -1,
        start_angle,
        &mut last_pointer_angle,
        fate_number,
        false,
        true,
        true,
    );

    let mut total_frames = ROULETTE_SPIN_DURATION_MS / ROULETTE_SPIN_FRAME_DELAY_MS;
    if total_frames < 1 {
        total_frames = 1;
    }

    for frame in 1..=total_frames {
        let p_q16 = ((frame as u32) << 16) / (total_frames as u32);
        let eased_q16 = (p_q16 * (131072u32 - p_q16)) >> 16; // p * (2 - p)
        let pointer_angle_frame = start_angle + ((total_rotation as i64 * eased_q16 as i64) >> 16) as i32;
        render_wheel_frame(
            backend,
            width,
            height,
            center_x,
            center_y,
            radius,
            -1,
            pointer_angle_frame,
            &mut last_pointer_angle,
            fate_number,
            false,
            false,
            false,
        );
        unsafe {
            backend_sleep_ms(backend, ROULETTE_SPIN_FRAME_DELAY_MS as u32);
        }
    }

    let pointer_angle = start_angle + total_rotation;
    let landing_segment = target_segment;
    render_wheel_frame(
        backend,
        width,
        height,
        center_x,
        center_y,
        radius,
        landing_segment,
        pointer_angle,
        &mut last_pointer_angle,
        fate_number,
        false,
        true,
        true,
    );
    unsafe {
        backend_sleep_ms(backend, 500);
        backend_sleep_ms(backend, 400);
    }

    for flash in 0..5 {
        render_wheel_frame(
            backend,
            width,
            height,
            center_x,
            center_y,
            radius,
            landing_segment,
            pointer_angle,
            &mut last_pointer_angle,
            fate_number,
            true,
            false,
            false,
        );
        unsafe {
            backend_sleep_ms(backend, 250);
        }
        if flash < 4 {
            render_wheel_frame(
                backend,
                width,
                height,
                center_x,
                center_y,
                radius,
                landing_segment,
                pointer_angle,
                &mut last_pointer_angle,
                fate_number,
                false,
                false,
                false,
            );
            unsafe {
                backend_sleep_ms(backend, 150);
            }
        }
    }

    render_wheel_frame(
        backend,
        width,
        height,
        center_x,
        center_y,
        radius,
        landing_segment,
        pointer_angle,
        &mut last_pointer_angle,
        fate_number,
        true,
        false,
        true,
    );
    unsafe {
        backend_sleep_ms(backend, 600);
    }

    let mut info_y = center_y + radius + 60;
    if info_y < 0 {
        info_y = 0;
    }
    if info_y > height {
        info_y = height;
    }
    unsafe {
        backend_fill_rect(backend, 0, info_y, width, height - info_y, ROULETTE_BG_COLOR);
    }
    draw_result_banner(backend, center_x, center_y + radius + 80, fate_number);

    let currency_text = if fate_number & 1 != 0 {
        TEXT_CURRENCY_WIN
    } else {
        TEXT_CURRENCY_LOSE
    };
    unsafe {
        backend_draw_text(backend, center_x - 110, center_y + radius + 170, currency_text, ROULETTE_TEXT_COLOR, 0);
    }

    if fate_number & 1 == 0 {
        unsafe {
            backend_draw_text(backend, center_x - 130, center_y + radius + 210, TEXT_RESET, 0xFFFF_00FF, 0);
        }
    } else {
        unsafe {
            backend_draw_text(backend, center_x - 130, center_y + radius + 210, TEXT_CONTINUE, 0x00FF_00FF, 0);
        }
    }

    unsafe {
        backend_sleep_ms(backend, ROULETTE_RESULT_DELAY_MS);
    }

    if fate_number & 1 != 0 {
        roulette_handoff_to_demo(backend, width, height);
    }

    0
}

