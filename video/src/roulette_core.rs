use core::ffi::{c_char, c_void};
use slopos_drivers::video_bridge::{VideoError, VideoResult};
use slopos_drivers::pit::pit_poll_delay_ms;

use crate::{framebuffer, font, graphics};
use crate::graphics::GraphicsContext;

const ROULETTE_BLANK_COLOR: u32 = 0x1E1E_1EFF;
const ROULETTE_BLANK_HIGHLIGHT: u32 = 0x3333_33FF;
const ROULETTE_COLORED_HIGHLIGHT: u32 = 0x3DD6_C6FF;
const ROULETTE_POINTER_COLOR: u32 = 0xE6E6_E6FF;
const ROULETTE_INFO_BG_COLOR: u32 = 0x1A1A_1AFF;

const ROULETTE_SEGMENT_COUNT: i32 = 12;
const ROULETTE_TRIG_SCALE: i32 = 1024;
const ROULETTE_WHEEL_RADIUS: i32 = 120;
const ROULETTE_INNER_RADIUS: i32 = 36;
const ROULETTE_DEGREE_STEPS: i32 = 360;
const ROULETTE_SEGMENT_DEGREES: i32 = 360 / ROULETTE_SEGMENT_COUNT;
const ROULETTE_SPIN_LOOPS: i32 = 4;
const ROULETTE_SPIN_DURATION_MS: i32 = 7200;
const ROULETTE_SPIN_FRAME_DELAY_MS: i32 = 12;

// Public colors pulled from the legacy header.
pub const ROULETTE_BG_COLOR: u32 = 0x0000_0000;
pub const ROULETTE_WHEEL_COLOR: u32 = 0xC7C7_C7FF;
pub const ROULETTE_TEXT_COLOR: u32 = 0xE6E6_E6FF;
pub const ROULETTE_WIN_COLOR: u32 = 0x2DD4_B3FF;
pub const ROULETTE_LOSE_COLOR: u32 = 0xE35D_5BFF;
pub const ROULETTE_EVEN_COLOR: u32 = 0x2A2A_2AFF;
pub const ROULETTE_ODD_COLOR: u32 = 0x144E_44FF;
pub const ROULETTE_RESULT_DELAY_MS: u32 = 5000;

#[repr(C)]
pub struct RouletteBackend {
    pub ctx: *mut c_void,
    pub get_size: Option<fn(*mut c_void, *mut i32, *mut i32) -> VideoResult>,
    pub fill_rect: Option<fn(*mut c_void, i32, i32, i32, i32, u32) -> VideoResult>,
    pub draw_line: Option<fn(*mut c_void, i32, i32, i32, i32, u32) -> VideoResult>,
    pub draw_circle: Option<fn(*mut c_void, i32, i32, i32, u32) -> VideoResult>,
    pub draw_circle_filled: Option<fn(*mut c_void, i32, i32, i32, u32) -> VideoResult>,
    pub draw_text: Option<fn(*mut c_void, i32, i32, *const u8, u32, u32) -> VideoResult>,
    pub sleep_ms: Option<fn(*mut c_void, u32)>,
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
const TEXT_WHEEL_TITLE: &[u8] = b"=== THE WHEEL OF FATE ===\0";
const TEXT_WHEEL_SUB: &[u8] = b"Pointers choose your destiny...\0";
const TEXT_CURRENCY_WIN: &[u8] = b"+10 W's (currency units)\0";
const TEXT_CURRENCY_LOSE: &[u8] = b"-10 W's (currency units)\0";
const TEXT_RESET: &[u8] = b"Press RESET to try again...\0";
const TEXT_CONTINUE: &[u8] = b"Continuing to OS...\0";

// Precomputed trig tables (scaled by 1024) carried over from the original C.
const COS_TABLE: [i16; (ROULETTE_SEGMENT_COUNT + 1) as usize] = [
    1024, 887, 512, 0, -512, -887, -1024, -887, -512, 0, 512, 887, 1024,
];
const SIN_TABLE: [i16; (ROULETTE_SEGMENT_COUNT + 1) as usize] = [
    0, 512, 887, 1024, 887, 512, 0, -512, -887, -1024, -887, -512, 0,
];

const COS360: [i16; ROULETTE_DEGREE_STEPS as usize] = [
    1024, 1024, 1023, 1023, 1022, 1020, 1018, 1016, 1014, 1011, 1008, 1005, 1002, 998, 994, 989,
    984, 979, 974, 968, 962, 956, 949, 943, 935, 928, 920, 912, 904, 896, 887, 878, 868, 859,
    849, 839, 828, 818, 807, 796, 784, 773, 761, 749, 737, 724, 711, 698, 685, 672, 658, 644,
    630, 616, 602, 587, 573, 558, 543, 527, 512, 496, 481, 465, 449, 433, 416, 400, 384, 367,
    350, 333, 316, 299, 282, 265, 248, 230, 213, 195, 178, 160, 143, 125, 107, 89, 71, 54, 36,
    18, 0, -18, -36, -54, -71, -89, -107, -125, -143, -160, -178, -195, -213, -230, -248, -265,
    -282, -299, -316, -333, -350, -367, -384, -400, -416, -433, -449, -465, -481, -496, -512,
    -527, -543, -558, -573, -587, -602, -616, -630, -644, -658, -672, -685, -698, -711, -724,
    -737, -749, -761, -773, -784, -796, -807, -818, -828, -839, -849, -859, -868, -878, -887,
    -896, -904, -912, -920, -928, -935, -943, -949, -956, -962, -968, -974, -979, -984, -989,
    -994, -998, -1002, -1005, -1008, -1011, -1014, -1016, -1018, -1020, -1022, -1023, -1023, -1024,
    -1024, -1024, -1023, -1023, -1022, -1020, -1018, -1016, -1014, -1011, -1008, -1005, -1002,
    -998, -994, -989, -984, -979, -974, -968, -962, -956, -949, -943, -935, -928, -920, -912,
    -904, -896, -887, -878, -868, -859, -849, -839, -828, -818, -807, -796, -784, -773, -761,
    -749, -737, -724, -711, -698, -685, -672, -658, -644, -630, -616, -602, -587, -573, -558,
    -543, -527, -512, -496, -481, -465, -449, -433, -416, -400, -384, -367, -350, -333, -316,
    -299, -282, -265, -248, -230, -213, -195, -178, -160, -143, -125, -107, -89, -71, -54, -36,
    -18, 0, 18, 36, 54, 71, 89, 107, 125, 143, 160, 178, 195, 213, 230, 248, 265, 282, 299, 316,
    333, 350, 367, 384, 400, 416, 433, 449, 465, 481, 496, 512, 527, 543, 558, 573, 587, 602,
    616, 630, 644, 658, 672, 685, 698, 711, 724, 737, 749, 761, 773, 784, 796, 807, 818, 828,
    839, 849, 859, 868, 878, 887, 896, 904, 912, 920, 928, 935, 943, 949, 956, 962, 968, 974,
    979, 984, 989, 994, 998, 1002, 1005, 1008, 1011, 1014, 1016, 1018, 1020, 1022, 1023, 1023,
    1024,
];

#[allow(clippy::unreadable_literal)]
const SIN360: [i16; ROULETTE_DEGREE_STEPS as usize] = [
    0, 18, 36, 54, 71, 89, 107, 125, 143, 160, 178, 195, 213, 230, 248, 265, 282, 299, 316, 333,
    350, 367, 384, 400, 416, 433, 449, 465, 481, 496, 512, 527, 543, 558, 573, 587, 602, 616,
    630, 644, 658, 672, 685, 698, 711, 724, 737, 749, 761, 773, 784, 796, 807, 818, 828, 839,
    849, 859, 868, 878, 887, 896, 904, 912, 920, 928, 935, 943, 949, 956, 962, 968, 974, 979,
    984, 989, 994, 998, 1002, 1005, 1008, 1011, 1014, 1016, 1018, 1020, 1022, 1023, 1023, 1024,
    1024, 1024, 1023, 1023, 1022, 1020, 1018, 1016, 1014, 1011, 1008, 1005, 1002, 998, 994, 989,
    984, 979, 974, 968, 962, 956, 949, 943, 935, 928, 920, 912, 904, 896, 887, 878, 868, 859,
    849, 839, 828, 818, 807, 796, 784, 773, 761, 749, 737, 724, 711, 698, 685, 672, 658, 644,
    630, 616, 602, 587, 573, 558, 543, 527, 512, 496, 481, 465, 449, 433, 416, 400, 384, 367,
    350, 333, 316, 299, 282, 265, 248, 230, 213, 195, 178, 160, 143, 125, 107, 89, 71, 54, 36,
    18, 0, -18, -36, -54, -71, -89, -107, -125, -143, -160, -178, -195, -213, -230, -248, -265,
    -282, -299, -316, -333, -350, -367, -384, -400, -416, -433, -449, -465, -481, -496, -512,
    -527, -543, -558, -573, -587, -602, -616, -630, -644, -658, -672, -685, -698, -711, -724,
    -737, -749, -761, -773, -784, -796, -807, -818, -828, -839, -849, -859, -868, -878, -887,
    -896, -904, -912, -920, -928, -935, -943, -949, -956, -962, -968, -974, -979, -984, -989,
    -994, -998, -1002, -1005, -1008, -1011, -1014, -1016, -1018, -1020, -1022, -1023, -1023, -1024,
    -1024, -1024, -1023, -1023, -1022, -1020, -1018, -1016, -1014, -1011, -1008, -1005, -1002,
    -998, -994, -989, -984, -979, -974, -968, -962, -956, -949, -943, -935, -928, -920, -912,
    -904, -896, -887, -878, -868, -859, -849, -839, -828, -818, -807, -796, -784, -773, -761,
    -749, -737, -724, -711, -698, -685, -672, -658, -644, -630, -616, -602, -587, -573, -558,
    -543, -527, -512, -496, -481, -465, -449, -433, -416, -400, -384, -367, -350, -333, -316,
    -299, -282, -265, -248, -230, -213, -195, -178, -160, -143, -125, -107, -89, -71, -54, -36,
    -18,
];

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

unsafe fn backend_get_size(b: &RouletteBackend, w: &mut i32, h: &mut i32) -> VideoResult {
    match b.get_size {
        Some(f) => f(b.ctx, w as *mut i32, h as *mut i32),
        None => Err(VideoError::NoFramebuffer),
    }
}

unsafe fn backend_fill_rect(
    b: &RouletteBackend,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) -> VideoResult {
    match b.fill_rect {
        Some(f) => f(b.ctx, x, y, w, h, color),
        None => Err(VideoError::NoFramebuffer),
    }
}

unsafe fn backend_draw_line(
    b: &RouletteBackend,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> VideoResult {
    match b.draw_line {
        Some(f) => f(b.ctx, x0, y0, x1, y1, color),
        None => Err(VideoError::NoFramebuffer),
    }
}

unsafe fn backend_draw_circle(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    match b.draw_circle {
        Some(f) => f(b.ctx, cx, cy, radius, color),
        None => Err(VideoError::NoFramebuffer),
    }
}

unsafe fn backend_draw_circle_filled(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    match b.draw_circle_filled {
        Some(f) => f(b.ctx, cx, cy, radius, color),
        None => Err(VideoError::NoFramebuffer),
    }
}

unsafe fn backend_draw_text(
    b: &RouletteBackend,
    x: i32,
    y: i32,
    text: &[u8],
    fg: u32,
    bg: u32,
) -> VideoResult {
    match b.draw_text {
        Some(f) => f(b.ctx, x, y, text.as_ptr(), fg, bg),
        None => Err(VideoError::NoFramebuffer),
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

struct RouletteLayout {
    radius: i32,
    inner_radius: i32,
    pointer_width: i32,
    pointer_tip_radius: i32,
    pointer_base_radius: i32,
}

fn draw_filled_triangle(
    b: &RouletteBackend,
    mut x0: i32,
    mut y0: i32,
    mut x1: i32,
    mut y1: i32,
    mut x2: i32,
    mut y2: i32,
    color: u32,
) -> VideoResult {
    if y0 > y1 {
        core::mem::swap(&mut y0, &mut y1);
        core::mem::swap(&mut x0, &mut x1);
    }
    if y1 > y2 {
        core::mem::swap(&mut y1, &mut y2);
        core::mem::swap(&mut x1, &mut x2);
    }
    if y0 > y1 {
        core::mem::swap(&mut y0, &mut y1);
        core::mem::swap(&mut x0, &mut x1);
    }

    let total_height = y2 - y0;
    if total_height == 0 {
        return Ok(());
    }

    for y in y0..=y2 {
        let second_half = y > y1 || y1 == y0;
        let segment_height = if second_half { y2 - y1 } else { y1 - y0 };
        if segment_height == 0 {
            continue;
        }
        let dy = y - if second_half { y1 } else { y0 };
        let alpha = (y - y0) as i64 * 65536 / total_height as i64;
        let beta = dy as i64 * 65536 / segment_height as i64;

        let ax = x0 + (((x2 - x0) as i64 * alpha) >> 16) as i32;
        let bx = if second_half {
            x1 + (((x2 - x1) as i64 * beta) >> 16) as i32
        } else {
            x0 + (((x1 - x0) as i64 * beta) >> 16) as i32
        };

        let (xa, xb) = if ax < bx { (ax, bx) } else { (bx, ax) };
        unsafe { backend_draw_line(b, xa, y, xb, y, color)? };
    }
    Ok(())
}

fn draw_segment_wedge(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    start_idx: usize,
    inner_radius: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    let inner = inner_radius;
    let start_deg = (start_idx as i32) * ROULETTE_SEGMENT_DEGREES;
    let end_deg = start_deg + ROULETTE_SEGMENT_DEGREES;

    let start_x = cx + scale(cos_deg(start_deg), radius);
    let start_y = cy + scale(sin_deg(start_deg), radius);
    let end_x = cx + scale(cos_deg(end_deg), radius);
    let end_y = cy + scale(sin_deg(end_deg), radius);

    draw_filled_triangle(b, cx, cy, start_x, start_y, end_x, end_y, color)?;

    if inner > 0 {
        let inner_start_x = cx + scale(cos_deg(start_deg), inner);
        let inner_start_y = cy + scale(sin_deg(start_deg), inner);
        let inner_end_x = cx + scale(cos_deg(end_deg), inner);
        let inner_end_y = cy + scale(sin_deg(end_deg), inner);
        draw_filled_triangle(
            b,
            cx,
            cy,
            inner_start_x,
            inner_start_y,
            inner_end_x,
            inner_end_y,
            ROULETTE_BG_COLOR,
        )?;
    }
    Ok(())
}

fn draw_segment_divider(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    idx: usize,
    radius: i32,
) -> VideoResult {
    let x_outer = cx + scale(COS_TABLE[idx], radius + 2);
    let y_outer = cy + scale(SIN_TABLE[idx], radius + 2);
    unsafe { backend_draw_line(b, cx, cy, x_outer, y_outer, ROULETTE_WHEEL_COLOR) }
}

fn draw_roulette_wheel(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    inner_radius: i32,
    radius: i32,
    highlight_segment: i32,
) -> VideoResult {
    unsafe {
        backend_draw_circle_filled(b, cx, cy, radius + 8, ROULETTE_BG_COLOR)?;
        backend_draw_circle(b, cx, cy, radius + 8, ROULETTE_WHEEL_COLOR)?;
    }

    for i in 0..ROULETTE_SEGMENT_COUNT {
        let is_colored = SEGMENTS[i as usize].is_colored;
        let mut base_color = if is_colored {
            ROULETTE_ODD_COLOR
        } else {
            ROULETTE_BLANK_COLOR
        };
        if i == highlight_segment {
            base_color = if is_colored {
                ROULETTE_COLORED_HIGHLIGHT
            } else {
                ROULETTE_BLANK_HIGHLIGHT
            };
        }
        draw_segment_wedge(b, cx, cy, i as usize, inner_radius, radius, base_color)?;
        draw_segment_divider(b, cx, cy, i as usize, radius)?;
    }
    draw_segment_divider(b, cx, cy, ROULETTE_SEGMENT_COUNT as usize, radius)?;

    unsafe {
        backend_draw_circle_filled(b, cx, cy, inner_radius + 6, ROULETTE_WHEEL_COLOR)?;
        backend_draw_circle_filled(b, cx, cy, inner_radius, ROULETTE_BG_COLOR)?;
    }
    Ok(())
}

fn draw_pointer_for_angle(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    pointer_width: i32,
    tip_radius: i32,
    base_radius: i32,
    angle_deg: i32,
    color: u32,
) -> VideoResult {
    let dir_x = cos_deg(angle_deg);
    let dir_y = sin_deg(angle_deg);
    let perp_x = -dir_y;
    let perp_y = dir_x;

    let tip_x = cx + scale(dir_x, tip_radius);
    let tip_y = cy + scale(dir_y, tip_radius);
    let base_x = cx + scale(dir_x, base_radius);
    let base_y = cy + scale(dir_y, base_radius);

    let offset_x = scale(perp_x, pointer_width);
    let offset_y = scale(perp_y, pointer_width);

    let left_x = base_x + offset_x;
    let left_y = base_y + offset_y;
    let right_x = base_x - offset_x;
    let right_y = base_y - offset_y;

    unsafe {
        backend_draw_line(b, tip_x, tip_y, left_x, left_y, color)?;
        backend_draw_line(b, tip_x, tip_y, right_x, right_y, color)?;
        backend_draw_line(b, left_x, left_y, right_x, right_y, color)?;
    }
    Ok(())
}

fn draw_pointer_ticks(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    pointer_width: i32,
    tip_radius: i32,
    base_radius: i32,
    angle_deg: i32,
    color: u32,
) {
    let _ = draw_pointer_for_angle(
        b,
        cx,
        cy,
        pointer_width,
        tip_radius,
        base_radius,
        angle_deg,
        color,
    );
    let _ = draw_pointer_for_angle(
        b,
        cx,
        cy,
        pointer_width,
        tip_radius,
        base_radius,
        angle_deg + 180,
        color,
    );
}

fn draw_fate_number(b: &RouletteBackend, cx: i32, y_pos: i32, fate_number: u32, revealed: bool) {
    if !revealed {
        unsafe {
            let _ = backend_fill_rect(b, cx - 100, y_pos, 200, 60, ROULETTE_INFO_BG_COLOR);
            let _ = backend_draw_line(b, cx - 100, y_pos, cx + 100, y_pos, ROULETTE_WHEEL_COLOR);
            let _ = backend_draw_line(
                b,
                cx - 100,
                y_pos + 60,
                cx + 100,
                y_pos + 60,
                ROULETTE_WHEEL_COLOR,
            );
            let _ =
                backend_draw_text(b, cx - 40, y_pos + 20, TEXT_UNKNOWN, ROULETTE_TEXT_COLOR, 0);
        }
        return;
    }

    let box_color = if fate_number & 1 == 1 {
        ROULETTE_ODD_COLOR
    } else {
        ROULETTE_EVEN_COLOR
    };
    unsafe {
        let _ = backend_fill_rect(b, cx - 100, y_pos, 200, 60, box_color);
        let _ = backend_draw_line(b, cx - 100, y_pos, cx + 100, y_pos, ROULETTE_WHEEL_COLOR);
        let _ = backend_draw_line(
            b,
            cx - 100,
            y_pos + 60,
            cx + 100,
            y_pos + 60,
            ROULETTE_WHEEL_COLOR,
        );
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
    let mut text_len = len;
    if len < num_str.len() {
        num_str[len] = 0;
        text_len = len + 1;
    }
    unsafe {
        let _ = backend_draw_text(
            b,
            text_x,
            y_pos + 20,
            &num_str[..text_len],
            ROULETTE_TEXT_COLOR,
            0,
        );
    }
}

fn draw_result_banner(b: &RouletteBackend, cx: i32, y_pos: i32, fate_number: u32) {
    let (result_text, sub_text, banner_color) = if fate_number & 1 == 1 {
        (TEXT_WIN, TEXT_WIN_SUB, ROULETTE_WIN_COLOR)
    } else {
        (TEXT_LOSE, TEXT_LOSE_SUB, ROULETTE_LOSE_COLOR)
    };

    unsafe {
        let _ = backend_fill_rect(b, cx - 200, y_pos, 400, 80, banner_color);
        let _ = backend_draw_line(
            b,
            cx - 202,
            y_pos - 2,
            cx + 202,
            y_pos - 2,
            ROULETTE_WHEEL_COLOR,
        );
        let _ = backend_draw_line(
            b,
            cx - 202,
            y_pos + 82,
            cx + 202,
            y_pos + 82,
            ROULETTE_WHEEL_COLOR,
        );
        let _ = backend_draw_text(b, cx - 60, y_pos + 15, result_text, ROULETTE_TEXT_COLOR, 0);
        let _ = backend_draw_text(b, cx - 140, y_pos + 50, sub_text, ROULETTE_TEXT_COLOR, 0);
    }
}

fn render_wheel_frame(
    b: &RouletteBackend,
    screen_width: i32,
    screen_height: i32,
    cx: i32,
    cy: i32,
    inner_radius: i32,
    radius: i32,
    pointer_width: i32,
    pointer_tip_radius: i32,
    pointer_base_radius: i32,
    highlight_segment: i32,
    pointer_angle_deg: i32,
    last_pointer_angle: &mut i32,
    fate_number: u32,
    reveal_number: bool,
    clear_background: bool,
    draw_wheel: bool,
) -> VideoResult {
    let region = radius + 80;
    let mut region_x = cx - region;
    let mut region_y = cy - region;
    let mut region_w = region * 2;
    let mut region_h = region * 2;

    if !clear_background && *last_pointer_angle >= 0 {
        draw_pointer_ticks(
            b,
            cx,
            cy,
            pointer_width,
            pointer_tip_radius,
            pointer_base_radius,
            *last_pointer_angle,
            ROULETTE_BG_COLOR,
        );
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
            backend_fill_rect(b, region_x, region_y, region_w, region_h, ROULETTE_BG_COLOR)?;
        }
    }

    if draw_wheel {
        draw_roulette_wheel(b, cx, cy, inner_radius, radius, highlight_segment)?;
    }
    draw_pointer_ticks(
        b,
        cx,
        cy,
        pointer_width,
        pointer_tip_radius,
        pointer_base_radius,
        pointer_angle_deg,
        ROULETTE_POINTER_COLOR,
    );
    draw_fate_number(b, cx, cy + radius + 30, fate_number, reveal_number);

    *last_pointer_angle = pointer_angle_deg;
    Ok(())
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

fn text_width_px(text: &[u8]) -> i32 {
    let mut len = 0usize;
    for &b in text {
        if b == 0 {
            break;
        }
        len += 1;
    }
    len as i32 * font::FONT_CHAR_WIDTH
}

pub fn roulette_run(backend: *const RouletteBackend, fate_number: u32) -> i32 {
    if backend.is_null() {
        return -1;
    }
    let backend = unsafe { &*backend };

    let mut width = 0;
    let mut height = 0;
    if unsafe { backend_get_size(backend, &mut width, &mut height) }.is_err()
        || width <= 0
        || height <= 0
    {
        return -2;
    }

    if unsafe { backend_fill_rect(backend, 0, 0, width, height, ROULETTE_BG_COLOR) }.is_err() {
        return -3;
    }

    let title_x = width / 2 - text_width_px(TEXT_WHEEL_TITLE) / 2;
    let sub_x = width / 2 - text_width_px(TEXT_WHEEL_SUB) / 2;
    unsafe {
        let _ = backend_draw_text(
            backend,
            title_x,
            50,
            TEXT_WHEEL_TITLE,
            ROULETTE_WHEEL_COLOR,
            0,
        );
        let _ = backend_draw_text(
            backend,
            sub_x,
            80,
            TEXT_WHEEL_SUB,
            ROULETTE_TEXT_COLOR,
            0,
        );
    }

    let mut radius = ROULETTE_WHEEL_RADIUS;
    let max_radius = (width.min(height) / 2) - 60;
    if radius > max_radius {
        radius = max_radius;
    }
    if radius < ROULETTE_INNER_RADIUS + 20 {
        radius = ROULETTE_INNER_RADIUS + 20;
    }

    let inner_radius = (radius * 3 / 10).clamp(ROULETTE_INNER_RADIUS, radius - 24);
    let pointer_width = (radius / 6).clamp(10, 18);
    let pointer_base_radius = radius + (radius / 8).clamp(6, 18);
    let pointer_tip_radius = radius + (radius / 3).clamp(18, 40);
    let layout = RouletteLayout {
        radius,
        inner_radius,
        pointer_width,
        pointer_tip_radius,
        pointer_base_radius,
    };

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
    let _ = render_wheel_frame(
        backend,
        width,
        height,
        center_x,
        center_y,
        layout.inner_radius,
        layout.radius,
        layout.pointer_width,
        layout.pointer_tip_radius,
        layout.pointer_base_radius,
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
        let eased_q16 = (((p_q16 as u64) * (131072u64 - p_q16 as u64)) >> 16) as u32; // p * (2 - p)
        let pointer_angle_frame =
            start_angle + ((total_rotation as i64 * eased_q16 as i64) >> 16) as i32;
        let _ = render_wheel_frame(
            backend,
            width,
            height,
            center_x,
            center_y,
            layout.inner_radius,
            layout.radius,
            layout.pointer_width,
            layout.pointer_tip_radius,
            layout.pointer_base_radius,
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
    let _ = render_wheel_frame(
        backend,
        width,
        height,
        center_x,
        center_y,
        layout.inner_radius,
        layout.radius,
        layout.pointer_width,
        layout.pointer_tip_radius,
        layout.pointer_base_radius,
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
        let _ = render_wheel_frame(
            backend,
            width,
            height,
            center_x,
            center_y,
            layout.inner_radius,
            layout.radius,
            layout.pointer_width,
            layout.pointer_tip_radius,
            layout.pointer_base_radius,
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
            let _ = render_wheel_frame(
                backend,
                width,
                height,
                center_x,
                center_y,
                layout.inner_radius,
                layout.radius,
                layout.pointer_width,
                layout.pointer_tip_radius,
                layout.pointer_base_radius,
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

    let _ = render_wheel_frame(
        backend,
        width,
        height,
        center_x,
        center_y,
        layout.inner_radius,
        layout.radius,
        layout.pointer_width,
        layout.pointer_tip_radius,
        layout.pointer_base_radius,
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

    let fate_box_y = center_y + layout.radius + 30;
    let fate_box_h = 60;
    let mut info_y = fate_box_y + fate_box_h + 10;
    if info_y < 0 {
        info_y = 0;
    }
    if info_y > height {
        info_y = height;
    }
    unsafe {
        let _ = backend_fill_rect(
            backend,
            0,
            info_y,
            width,
            height - info_y,
            ROULETTE_BG_COLOR,
        );
    }
    let banner_y = info_y + 10;
    draw_result_banner(backend, center_x, banner_y, fate_number);

    let currency_text = if fate_number & 1 != 0 {
        TEXT_CURRENCY_WIN
    } else {
        TEXT_CURRENCY_LOSE
    };
    unsafe {
        let _ = backend_draw_text(
            backend,
            center_x - 110,
            banner_y + 90,
            currency_text,
            ROULETTE_TEXT_COLOR,
            0,
        );
    }

    if fate_number & 1 == 0 {
        unsafe {
            let _ = backend_draw_text(
                backend,
                center_x - 130,
                banner_y + 130,
                TEXT_RESET,
                ROULETTE_TEXT_COLOR,
                0,
            );
        }
    } else {
        let continue_x = center_x - text_width_px(TEXT_CONTINUE) / 2;
        unsafe {
            let _ = backend_draw_text(
                backend,
                continue_x,
                banner_y + 130,
                TEXT_CONTINUE,
                ROULETTE_TEXT_COLOR,
                0,
            );
        }
    }

    unsafe {
        backend_sleep_ms(backend, ROULETTE_RESULT_DELAY_MS);
    }

    0
}

fn kernel_get_size(ctx: *mut c_void, w: *mut i32, h: *mut i32) -> VideoResult {
    if let Some(ctx) = unsafe { (ctx as *const GraphicsContext).as_ref() } {
        unsafe {
            if !w.is_null() {
                *w = ctx.width() as i32;
            }
            if !h.is_null() {
                *h = ctx.height() as i32;
            }
        }
        return Ok(());
    }

    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return Err(VideoError::NoFramebuffer),
    };
    unsafe {
        if !w.is_null() {
            *w = fb.width as i32;
        }
        if !h.is_null() {
            *h = fb.height as i32;
        }
    }
    Ok(())
}

fn kernel_fill_rect(
    ctx: *mut c_void,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) -> VideoResult {
    let ctx = unsafe { (ctx as *const GraphicsContext).as_ref() }
        .ok_or(VideoError::Invalid)?;
    graphics::graphics_draw_rect_filled_fast_ctx(ctx, x, y, w, h, color)
}

fn kernel_draw_line(
    ctx: *mut c_void,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> VideoResult {
    let ctx = unsafe { (ctx as *const GraphicsContext).as_ref() }
        .ok_or(VideoError::Invalid)?;
    graphics::graphics_draw_line_ctx(ctx, x0, y0, x1, y1, color)
}

fn kernel_draw_circle(
    ctx: *mut c_void,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    let ctx = unsafe { (ctx as *const GraphicsContext).as_ref() }
        .ok_or(VideoError::Invalid)?;
    graphics::graphics_draw_circle_ctx(ctx, cx, cy, radius, color)
}

fn kernel_draw_circle_filled(
    ctx: *mut c_void,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    let ctx = unsafe { (ctx as *const GraphicsContext).as_ref() }
        .ok_or(VideoError::Invalid)?;
    graphics::graphics_draw_circle_filled_ctx(ctx, cx, cy, radius, color)
}

fn kernel_draw_text(
    ctx: *mut c_void,
    x: i32,
    y: i32,
    text: *const u8,
    fg: u32,
    bg: u32,
) -> VideoResult {
    let ctx = unsafe { (ctx as *const GraphicsContext).as_ref() }
        .ok_or(VideoError::Invalid)?;
    if text.is_null() {
        return Err(VideoError::Invalid);
    }
    let rc = font::font_draw_string_ctx(ctx, x, y, text as *const c_char, fg, bg);
    if rc == 0 { Ok(()) } else { Err(VideoError::Invalid) }
}

fn kernel_sleep_ms(_ctx: *mut c_void, ms: u32) {
    // Flush after each frame so virtio-gpu scanout updates during animation.
    crate::framebuffer::framebuffer_flush();
    pit_poll_delay_ms(ms);
}

pub fn roulette_draw_kernel(fate_number: u32) -> VideoResult {
    let mut gfx_ctx = GraphicsContext::new()?;
    let backend = RouletteBackend {
        ctx: &mut gfx_ctx as *mut GraphicsContext as *mut c_void,
        get_size: Some(kernel_get_size),
        fill_rect: Some(kernel_fill_rect),
        draw_line: Some(kernel_draw_line),
        draw_circle: Some(kernel_draw_circle),
        draw_circle_filled: Some(kernel_draw_circle_filled),
        draw_text: Some(kernel_draw_text),
        sleep_ms: Some(kernel_sleep_ms),
    };

    let result = roulette_run(&backend as *const RouletteBackend, fate_number);

    if result == 0 {
        Ok(())
    } else {
        Err(VideoError::Invalid)
    }
}
