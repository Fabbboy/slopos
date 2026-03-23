//! Roulette wheel animation for the Wheel of Fate.
//!
//! Draws a spinning roulette wheel on the framebuffer and reveals the fate
//! number that determines the boot outcome. Odd numbers are wins (colored
//! segments), even numbers are losses (blank segments).
//!
//! All drawing is performed through the [`Canvas`] trait, which the kernel's
//! [`GraphicsContext`] already implements for volatile MMIO framebuffer writes.
//! No vtable, no `*mut c_void`, no `Option<fn(...)>` indirection — just
//! monomorphised generic calls.

use slopos_abi::draw::{Canvas, Color32};
use slopos_abi::video_traits::{VideoError, VideoResult};
use slopos_drivers::hpet;
use slopos_gfx::canvas_ops;
use slopos_utils::numfmt;

use crate::graphics::GraphicsContext;
use crate::kernel_font;

// ---------------------------------------------------------------------------
// Color palette
// ---------------------------------------------------------------------------

const BLANK_COLOR: Color32 = Color32(0x2A31_3BFF);
const BLANK_HIGHLIGHT: Color32 = Color32(0x5660_70FF);
const COLORED_HIGHLIGHT: Color32 = Color32(0x4DE3_CAFF);
const POINTER_COLOR: Color32 = Color32(0xFFE0_87FF);
const INFO_BG: Color32 = Color32(0x0E14_1CFF);
const CARD_BORDER: Color32 = Color32(0x6470_83FF);
const CARD_TEXT: Color32 = Color32(0xF6F9_FDFF);
const MUTED_TEXT: Color32 = Color32(0xAAB4_C5FF);
const WIN_BG: Color32 = Color32(0x0D37_31FF);
const LOSE_BG: Color32 = Color32(0x3A20_24FF);

const BG_COLOR: Color32 = Color32(0x0000_0000);
const WHEEL_COLOR: Color32 = Color32(0xD4DB_E6FF);
const TEXT_COLOR: Color32 = Color32(0xF1F4_F9FF);
const ODD_COLOR: Color32 = Color32(0x0C7A_68FF);
const EVEN_COLOR: Color32 = Color32(0x3B45_54FF);

const RESULT_DELAY_MS: u32 = 1700;

// ---------------------------------------------------------------------------
// Wheel geometry constants
// ---------------------------------------------------------------------------

const SEGMENT_COUNT: i32 = 12;
const TRIG_SCALE: i32 = 1024;
const WHEEL_RADIUS: i32 = 120;
const INNER_RADIUS: i32 = 36;
const DEGREE_STEPS: i32 = 360;
const SEGMENT_DEGREES: i32 = DEGREE_STEPS / SEGMENT_COUNT;
const SPIN_LOOPS: i32 = 4;
const SPIN_DURATION_MS: i32 = 7200;
const SPIN_FRAME_DELAY_MS: i32 = 12;

// ---------------------------------------------------------------------------
// Text constants (null-terminated for atlas byte-string rendering)
// ---------------------------------------------------------------------------

const TEXT_UNKNOWN: &[u8] = b"Hidden\0";
const TEXT_WIN: &[u8] = b"Win\0";
const TEXT_WIN_SUB: &[u8] = b"Boot path unlocked\0";
const TEXT_LOSE: &[u8] = b"Lose\0";
const TEXT_LOSE_SUB: &[u8] = b"Retry after reboot\0";
const TEXT_CARD_LABEL: &[u8] = b"Number\0";
const TEXT_PARITY_ODD: &[u8] = b"Odd\0";
const TEXT_PARITY_EVEN: &[u8] = b"Even\0";
const TEXT_CURRENCY_WIN: &[u8] = b"+10 W\0";
const TEXT_CURRENCY_LOSE: &[u8] = b"-10 W\0";
const TEXT_BOOTING_TITLE: &[u8] = b"Starting SlopOS\0";
const TEXT_BOOTING_SUB: &[u8] = b"Loading shell and compositor\0";
const TEXT_BOOTING_DETAIL: &[u8] = b"Please wait\0";
const TEXT_REBOOT_TITLE: &[u8] = b"Rebooting\0";
const TEXT_REBOOT_SUB: &[u8] = b"Preparing next spin\0";
const TEXT_REBOOT_DETAIL: &[u8] = b"Please wait\0";
const SPINNER_FRAMES: [&[u8]; 4] = [b"|\0", b"/\0", b"-\0", b"\\\0"];

// ---------------------------------------------------------------------------
// Precomputed trigonometry (scaled by TRIG_SCALE = 1024)
// ---------------------------------------------------------------------------

const COS_TABLE: [i16; (SEGMENT_COUNT + 1) as usize] = [
    1024, 887, 512, 0, -512, -887, -1024, -887, -512, 0, 512, 887, 1024,
];

const SIN_TABLE: [i16; (SEGMENT_COUNT + 1) as usize] = [
    0, 512, 887, 1024, 887, 512, 0, -512, -887, -1024, -887, -512, 0,
];

const COS360: [i16; DEGREE_STEPS as usize] = [
    1024, 1024, 1023, 1023, 1022, 1020, 1018, 1016, 1014, 1011, 1008, 1005, 1002, 998, 994, 989,
    984, 979, 974, 968, 962, 956, 949, 943, 935, 928, 920, 912, 904, 896, 887, 878, 868, 859, 849,
    839, 828, 818, 807, 796, 784, 773, 761, 749, 737, 724, 711, 698, 685, 672, 658, 644, 630, 616,
    602, 587, 573, 558, 543, 527, 512, 496, 481, 465, 449, 433, 416, 400, 384, 367, 350, 333, 316,
    299, 282, 265, 248, 230, 213, 195, 178, 160, 143, 125, 107, 89, 71, 54, 36, 18, 0, -18, -36,
    -54, -71, -89, -107, -125, -143, -160, -178, -195, -213, -230, -248, -265, -282, -299, -316,
    -333, -350, -367, -384, -400, -416, -433, -449, -465, -481, -496, -512, -527, -543, -558, -573,
    -587, -602, -616, -630, -644, -658, -672, -685, -698, -711, -724, -737, -749, -761, -773, -784,
    -796, -807, -818, -828, -839, -849, -859, -868, -878, -887, -896, -904, -912, -920, -928, -935,
    -943, -949, -956, -962, -968, -974, -979, -984, -989, -994, -998, -1002, -1005, -1008, -1011,
    -1014, -1016, -1018, -1020, -1022, -1023, -1023, -1024, -1024, -1024, -1023, -1023, -1022,
    -1020, -1018, -1016, -1014, -1011, -1008, -1005, -1002, -998, -994, -989, -984, -979, -974,
    -968, -962, -956, -949, -943, -935, -928, -920, -912, -904, -896, -887, -878, -868, -859, -849,
    -839, -828, -818, -807, -796, -784, -773, -761, -749, -737, -724, -711, -698, -685, -672, -658,
    -644, -630, -616, -602, -587, -573, -558, -543, -527, -512, -496, -481, -465, -449, -433, -416,
    -400, -384, -367, -350, -333, -316, -299, -282, -265, -248, -230, -213, -195, -178, -160, -143,
    -125, -107, -89, -71, -54, -36, -18, 0, 18, 36, 54, 71, 89, 107, 125, 143, 160, 178, 195, 213,
    230, 248, 265, 282, 299, 316, 333, 350, 367, 384, 400, 416, 433, 449, 465, 481, 496, 512, 527,
    543, 558, 573, 587, 602, 616, 630, 644, 658, 672, 685, 698, 711, 724, 737, 749, 761, 773, 784,
    796, 807, 818, 828, 839, 849, 859, 868, 878, 887, 896, 904, 912, 920, 928, 935, 943, 949, 956,
    962, 968, 974, 979, 984, 989, 994, 998, 1002, 1005, 1008, 1011, 1014, 1016, 1018, 1020, 1022,
    1023, 1023, 1024,
];

#[allow(clippy::unreadable_literal)]
const SIN360: [i16; DEGREE_STEPS as usize] = [
    0, 18, 36, 54, 71, 89, 107, 125, 143, 160, 178, 195, 213, 230, 248, 265, 282, 299, 316, 333,
    350, 367, 384, 400, 416, 433, 449, 465, 481, 496, 512, 527, 543, 558, 573, 587, 602, 616, 630,
    644, 658, 672, 685, 698, 711, 724, 737, 749, 761, 773, 784, 796, 807, 818, 828, 839, 849, 859,
    868, 878, 887, 896, 904, 912, 920, 928, 935, 943, 949, 956, 962, 968, 974, 979, 984, 989, 994,
    998, 1002, 1005, 1008, 1011, 1014, 1016, 1018, 1020, 1022, 1023, 1023, 1024, 1024, 1024, 1023,
    1023, 1022, 1020, 1018, 1016, 1014, 1011, 1008, 1005, 1002, 998, 994, 989, 984, 979, 974, 968,
    962, 956, 949, 943, 935, 928, 920, 912, 904, 896, 887, 878, 868, 859, 849, 839, 828, 818, 807,
    796, 784, 773, 761, 749, 737, 724, 711, 698, 685, 672, 658, 644, 630, 616, 602, 587, 573, 558,
    543, 527, 512, 496, 481, 465, 449, 433, 416, 400, 384, 367, 350, 333, 316, 299, 282, 265, 248,
    230, 213, 195, 178, 160, 143, 125, 107, 89, 71, 54, 36, 18, 0, -18, -36, -54, -71, -89, -107,
    -125, -143, -160, -178, -195, -213, -230, -248, -265, -282, -299, -316, -333, -350, -367, -384,
    -400, -416, -433, -449, -465, -481, -496, -512, -527, -543, -558, -573, -587, -602, -616, -630,
    -644, -658, -672, -685, -698, -711, -724, -737, -749, -761, -773, -784, -796, -807, -818, -828,
    -839, -849, -859, -868, -878, -887, -896, -904, -912, -920, -928, -935, -943, -949, -956, -962,
    -968, -974, -979, -984, -989, -994, -998, -1002, -1005, -1008, -1011, -1014, -1016, -1018,
    -1020, -1022, -1023, -1023, -1024, -1024, -1024, -1023, -1023, -1022, -1020, -1018, -1016,
    -1014, -1011, -1008, -1005, -1002, -998, -994, -989, -984, -979, -974, -968, -962, -956, -949,
    -943, -935, -928, -920, -912, -904, -896, -887, -878, -868, -859, -849, -839, -828, -818, -807,
    -796, -784, -773, -761, -749, -737, -724, -711, -698, -685, -672, -658, -644, -630, -616, -602,
    -587, -573, -558, -543, -527, -512, -496, -481, -465, -449, -433, -416, -400, -384, -367, -350,
    -333, -316, -299, -282, -265, -248, -230, -213, -195, -178, -160, -143, -125, -107, -89, -71,
    -54, -36, -18,
];

// ---------------------------------------------------------------------------
// Trigonometry helpers
// ---------------------------------------------------------------------------

fn normalize_angle(degrees: i32) -> i32 {
    let mut angle = degrees % DEGREE_STEPS;
    if angle < 0 {
        angle += DEGREE_STEPS;
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
    (value as i32 * radius) / TRIG_SCALE
}

// ---------------------------------------------------------------------------
// Segment helpers
// ---------------------------------------------------------------------------

const fn is_colored_segment(index: i32) -> bool {
    (index % 2) == 0
}

fn segment_center_angle(index: i32) -> i32 {
    index * SEGMENT_DEGREES + (SEGMENT_DEGREES / 2)
}

fn choose_landing_segment(fate_number: u32, need_colored: bool) -> i32 {
    let start = (fate_number % SEGMENT_COUNT as u32) as i32;
    for offset in 0..SEGMENT_COUNT {
        let idx = (start + offset) % SEGMENT_COUNT;
        if is_colored_segment(idx) == need_colored {
            return idx;
        }
    }
    start
}

// ---------------------------------------------------------------------------
// Layout computation
// ---------------------------------------------------------------------------

struct WheelLayout {
    radius: i32,
    inner_radius: i32,
    pointer_width: i32,
    pointer_tip_radius: i32,
    pointer_base_radius: i32,
}

impl WheelLayout {
    fn compute(screen_w: i32, screen_h: i32) -> Self {
        let max_radius = (screen_w.min(screen_h) / 2) - 60;
        let radius = WHEEL_RADIUS.min(max_radius).max(INNER_RADIUS + 20);
        let inner_radius = (radius * 3 / 10).clamp(INNER_RADIUS, radius - 24);
        let pointer_width = (radius / 6).clamp(10, 18);
        let pointer_base_radius = radius + (radius / 8).clamp(6, 18);
        let pointer_tip_radius = radius + (radius / 3).clamp(18, 40);
        Self {
            radius,
            inner_radius,
            pointer_width,
            pointer_tip_radius,
            pointer_base_radius,
        }
    }
}

// ---------------------------------------------------------------------------
// Text drawing helpers
// ---------------------------------------------------------------------------

fn even_px(x: i32) -> i32 {
    x & !1
}

fn draw_text_centered<T: Canvas>(ctx: &mut T, cx: i32, y: i32, text: &[u8], fg: Color32) {
    if let Some(atlas) = kernel_font::atlas() {
        let x = even_px(cx - atlas.bytes_width(text) / 2);
        atlas.draw_bytes(ctx, x, y, text, fg, Color32(0));
    }
}

// ---------------------------------------------------------------------------
// Panel drawing (fill + border)
// ---------------------------------------------------------------------------

fn draw_panel<T: Canvas>(
    ctx: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    fill: Color32,
    border: Color32,
) {
    canvas_ops::fill_rect(ctx, x, y, w, h, fill);
    canvas_ops::line(ctx, x, y, x + w, y, border);
    canvas_ops::line(ctx, x, y + h, x + w, y + h, border);
    canvas_ops::line(ctx, x, y, x, y + h, border);
    canvas_ops::line(ctx, x + w, y, x + w, y + h, border);
}

// ---------------------------------------------------------------------------
// Wheel segment drawing
// ---------------------------------------------------------------------------

fn draw_segment_wedge<T: Canvas>(
    ctx: &mut T,
    cx: i32,
    cy: i32,
    start_idx: usize,
    inner_radius: i32,
    radius: i32,
    color: Color32,
) {
    let start_deg = (start_idx as i32) * SEGMENT_DEGREES;
    let end_deg = start_deg + SEGMENT_DEGREES;

    let start_x = cx + scale(cos_deg(start_deg), radius);
    let start_y = cy + scale(sin_deg(start_deg), radius);
    let end_x = cx + scale(cos_deg(end_deg), radius);
    let end_y = cy + scale(sin_deg(end_deg), radius);

    canvas_ops::triangle_filled(ctx, cx, cy, start_x, start_y, end_x, end_y, color);

    if inner_radius > 0 {
        let inner_sx = cx + scale(cos_deg(start_deg), inner_radius);
        let inner_sy = cy + scale(sin_deg(start_deg), inner_radius);
        let inner_ex = cx + scale(cos_deg(end_deg), inner_radius);
        let inner_ey = cy + scale(sin_deg(end_deg), inner_radius);
        canvas_ops::triangle_filled(
            ctx, cx, cy, inner_sx, inner_sy, inner_ex, inner_ey, BG_COLOR,
        );
    }
}

fn draw_segment_divider<T: Canvas>(ctx: &mut T, cx: i32, cy: i32, idx: usize, radius: i32) {
    let x_outer = cx + scale(COS_TABLE[idx], radius + 2);
    let y_outer = cy + scale(SIN_TABLE[idx], radius + 2);
    canvas_ops::line(ctx, cx, cy, x_outer, y_outer, WHEEL_COLOR);
}

fn draw_roulette_wheel<T: Canvas>(
    ctx: &mut T,
    cx: i32,
    cy: i32,
    inner_radius: i32,
    radius: i32,
    highlight_segment: i32,
) {
    canvas_ops::circle_filled(ctx, cx, cy, radius + 8, BG_COLOR);
    canvas_ops::circle(ctx, cx, cy, radius + 8, WHEEL_COLOR);

    for i in 0..SEGMENT_COUNT {
        let colored = is_colored_segment(i);
        let base_color = if i == highlight_segment {
            if colored {
                COLORED_HIGHLIGHT
            } else {
                BLANK_HIGHLIGHT
            }
        } else if colored {
            ODD_COLOR
        } else {
            BLANK_COLOR
        };
        draw_segment_wedge(ctx, cx, cy, i as usize, inner_radius, radius, base_color);
        draw_segment_divider(ctx, cx, cy, i as usize, radius);
    }
    draw_segment_divider(ctx, cx, cy, SEGMENT_COUNT as usize, radius);

    canvas_ops::circle_filled(ctx, cx, cy, inner_radius + 6, WHEEL_COLOR);
    canvas_ops::circle_filled(ctx, cx, cy, inner_radius, BG_COLOR);
}

// ---------------------------------------------------------------------------
// Pointer drawing
// ---------------------------------------------------------------------------

fn draw_pointer_for_angle<T: Canvas>(
    ctx: &mut T,
    cx: i32,
    cy: i32,
    pointer_width: i32,
    tip_radius: i32,
    base_radius: i32,
    angle_deg: i32,
    color: Color32,
) {
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

    canvas_ops::line(ctx, tip_x, tip_y, left_x, left_y, color);
    canvas_ops::line(ctx, tip_x, tip_y, right_x, right_y, color);
    canvas_ops::line(ctx, left_x, left_y, right_x, right_y, color);
}

fn draw_pointer_ticks<T: Canvas>(
    ctx: &mut T,
    cx: i32,
    cy: i32,
    layout: &WheelLayout,
    angle_deg: i32,
    color: Color32,
) {
    draw_pointer_for_angle(
        ctx,
        cx,
        cy,
        layout.pointer_width,
        layout.pointer_tip_radius,
        layout.pointer_base_radius,
        angle_deg,
        color,
    );
    draw_pointer_for_angle(
        ctx,
        cx,
        cy,
        layout.pointer_width,
        layout.pointer_tip_radius,
        layout.pointer_base_radius,
        angle_deg + 180,
        color,
    );
}

// ---------------------------------------------------------------------------
// Fate number card
// ---------------------------------------------------------------------------

fn draw_fate_number<T: Canvas>(ctx: &mut T, cx: i32, y_pos: i32, fate_number: u32, revealed: bool) {
    let card_x = cx - 136;
    let card_w = 272;
    let card_h = 88;
    let number_y = y_pos + 46;

    draw_panel(ctx, card_x, y_pos, card_w, card_h, INFO_BG, CARD_BORDER);
    draw_text_centered(ctx, cx, y_pos + 14, TEXT_CARD_LABEL, MUTED_TEXT);

    if !revealed {
        draw_text_centered(ctx, cx, number_y, TEXT_UNKNOWN, CARD_TEXT);
        return;
    }

    let box_color = if fate_number & 1 == 1 {
        ODD_COLOR
    } else {
        EVEN_COLOR
    };
    canvas_ops::fill_rect(ctx, card_x + 8, y_pos + 34, card_w - 16, 44, box_color);

    let mut num_buf = numfmt::NumBuf::<12>::new();
    let num_text = num_buf.format_u32(fate_number);
    if let Some(atlas) = kernel_font::atlas() {
        let text_w = atlas.bytes_width(num_text);
        let text_x = cx - text_w / 2;
        atlas.draw_bytes(ctx, text_x, number_y, num_text, CARD_TEXT, Color32(0));
    }
}

// ---------------------------------------------------------------------------
// Result banner
// ---------------------------------------------------------------------------

fn draw_result_banner<T: Canvas>(ctx: &mut T, cx: i32, y_pos: i32, fate_number: u32) {
    let is_win = fate_number & 1 == 1;
    let (result_text, sub_text, parity_text, currency_text, banner_color) = if is_win {
        (
            TEXT_WIN,
            TEXT_WIN_SUB,
            TEXT_PARITY_ODD,
            TEXT_CURRENCY_WIN,
            WIN_BG,
        )
    } else {
        (
            TEXT_LOSE,
            TEXT_LOSE_SUB,
            TEXT_PARITY_EVEN,
            TEXT_CURRENCY_LOSE,
            LOSE_BG,
        )
    };

    draw_panel(ctx, cx - 220, y_pos, 440, 108, banner_color, CARD_BORDER);
    draw_text_centered(ctx, cx, y_pos + 16, result_text, CARD_TEXT);
    draw_text_centered(ctx, cx, y_pos + 41, sub_text, MUTED_TEXT);

    draw_panel(ctx, cx - 190, y_pos + 64, 160, 32, INFO_BG, CARD_BORDER);
    draw_panel(ctx, cx + 30, y_pos + 64, 160, 32, INFO_BG, CARD_BORDER);
    draw_text_centered(ctx, cx - 110, y_pos + 74, parity_text, TEXT_COLOR);
    draw_text_centered(ctx, cx + 110, y_pos + 74, currency_text, TEXT_COLOR);
}

// ---------------------------------------------------------------------------
// Frame parameters
// ---------------------------------------------------------------------------

struct FrameParams {
    highlight_segment: i32,
    pointer_angle_deg: i32,
    fate_number: u32,
    reveal_number: bool,
    clear_background: bool,
    draw_wheel: bool,
}

// ---------------------------------------------------------------------------
// Single-frame renderer
// ---------------------------------------------------------------------------

fn render_wheel_frame<T: Canvas>(
    ctx: &mut T,
    screen_w: i32,
    screen_h: i32,
    cx: i32,
    cy: i32,
    layout: &WheelLayout,
    last_pointer_angle: &mut i32,
    params: &FrameParams,
) {
    let region = layout.radius + 80;

    if !params.clear_background && *last_pointer_angle >= 0 {
        draw_pointer_ticks(ctx, cx, cy, layout, *last_pointer_angle, BG_COLOR);
    }

    if params.clear_background {
        let mut rx = cx - region;
        let mut ry = cy - region;
        let mut rw = region * 2;
        let mut rh = region * 2;
        if rx < 0 {
            rw += rx;
            rx = 0;
        }
        if ry < 0 {
            rh += ry;
            ry = 0;
        }
        if rx + rw > screen_w {
            rw = screen_w - rx;
        }
        if ry + rh > screen_h {
            rh = screen_h - ry;
        }
        canvas_ops::fill_rect(ctx, rx, ry, rw, rh, BG_COLOR);
    }

    if params.draw_wheel {
        draw_roulette_wheel(
            ctx,
            cx,
            cy,
            layout.inner_radius,
            layout.radius,
            params.highlight_segment,
        );
    }

    draw_pointer_ticks(ctx, cx, cy, layout, params.pointer_angle_deg, POINTER_COLOR);
    draw_fate_number(
        ctx,
        cx,
        cy + layout.radius + 30,
        params.fate_number,
        params.reveal_number,
    );

    *last_pointer_angle = params.pointer_angle_deg;
}

// ---------------------------------------------------------------------------
// Transition spinner (post-result screen)
// ---------------------------------------------------------------------------

fn draw_transition_spinner(ctx: &mut GraphicsContext, screen_w: i32, screen_h: i32, is_win: bool) {
    let panel_w = (screen_w - 120)
        .max(260)
        .min(520)
        .min((screen_w - 20).max(220));
    let panel_h = 152;
    let panel_x = (screen_w - panel_w) / 2;
    let panel_y = (screen_h - panel_h) / 2;
    let center_x = panel_x + panel_w / 2;

    let panel_bg = if is_win { WIN_BG } else { LOSE_BG };
    let title = if is_win {
        TEXT_BOOTING_TITLE
    } else {
        TEXT_REBOOT_TITLE
    };
    let subtitle = if is_win {
        TEXT_BOOTING_SUB
    } else {
        TEXT_REBOOT_SUB
    };
    let detail = if is_win {
        TEXT_BOOTING_DETAIL
    } else {
        TEXT_REBOOT_DETAIL
    };

    for tick in 0..16i32 {
        draw_panel(
            ctx,
            panel_x,
            panel_y,
            panel_w,
            panel_h,
            panel_bg,
            CARD_BORDER,
        );
        draw_text_centered(ctx, center_x, panel_y + 26, title, CARD_TEXT);
        draw_text_centered(ctx, center_x, panel_y + 56, subtitle, MUTED_TEXT);

        let spinner_box_x = panel_x + (panel_w - 120) / 2;
        canvas_ops::fill_rect(ctx, spinner_box_x, panel_y + 86, 120, 36, INFO_BG);
        canvas_ops::line(
            ctx,
            spinner_box_x,
            panel_y + 86,
            spinner_box_x + 120,
            panel_y + 86,
            CARD_BORDER,
        );
        canvas_ops::line(
            ctx,
            spinner_box_x,
            panel_y + 122,
            spinner_box_x + 120,
            panel_y + 122,
            CARD_BORDER,
        );
        draw_text_centered(
            ctx,
            center_x,
            panel_y + 96,
            SPINNER_FRAMES[(tick % SPINNER_FRAMES.len() as i32) as usize],
            POINTER_COLOR,
        );
        draw_text_centered(ctx, center_x, panel_y + 132, detail, TEXT_COLOR);

        ctx.flush();
        hpet::delay_ms(90);
    }
}

// ---------------------------------------------------------------------------
// Flush-and-sleep helper
// ---------------------------------------------------------------------------

fn flush_and_sleep(ctx: &GraphicsContext, ms: u32) {
    ctx.flush();
    hpet::delay_ms(ms);
}

// ---------------------------------------------------------------------------
// Main animation driver
// ---------------------------------------------------------------------------

fn run_animation(ctx: &mut GraphicsContext, fate_number: u32) -> VideoResult {
    let width = ctx.width() as i32;
    let height = ctx.height() as i32;
    if width <= 0 || height <= 0 {
        return Err(VideoError::NoFramebuffer);
    }

    canvas_ops::fill_rect(ctx, 0, 0, width, height, BG_COLOR);

    let layout = WheelLayout::compute(width, height);
    let center_x = width / 2;
    let center_y = height / 2;

    let want_colored = (fate_number & 1) != 0;
    let mut start_segment = (fate_number % SEGMENT_COUNT as u32) as i32;
    let target_segment = choose_landing_segment(fate_number, want_colored);
    if start_segment == target_segment {
        start_segment = (start_segment + 3) % SEGMENT_COUNT;
    }

    flush_and_sleep(ctx, 300);

    let start_angle = segment_center_angle(start_segment);
    let target_angle = segment_center_angle(target_segment);
    let rotation_to_target = normalize_angle(target_angle - start_angle);
    let mut total_rotation = SPIN_LOOPS * DEGREE_STEPS + rotation_to_target;
    if total_rotation <= 0 {
        total_rotation += DEGREE_STEPS;
    }

    let mut last_pointer_angle = -1i32;

    // Phase 1: Initial frame
    render_wheel_frame(
        ctx,
        width,
        height,
        center_x,
        center_y,
        &layout,
        &mut last_pointer_angle,
        &FrameParams {
            highlight_segment: -1,
            pointer_angle_deg: start_angle,
            fate_number,
            reveal_number: false,
            clear_background: true,
            draw_wheel: true,
        },
    );

    // Phase 2: Spinning animation (ease-out: p * (2 - p))
    let total_frames = (SPIN_DURATION_MS / SPIN_FRAME_DELAY_MS).max(1);
    for frame in 1..=total_frames {
        let p_q16 = ((frame as u32) << 16) / (total_frames as u32);
        let eased_q16 = (((p_q16 as u64) * (131072u64 - p_q16 as u64)) >> 16) as u32;
        let angle = start_angle + ((total_rotation as i64 * eased_q16 as i64) >> 16) as i32;

        render_wheel_frame(
            ctx,
            width,
            height,
            center_x,
            center_y,
            &layout,
            &mut last_pointer_angle,
            &FrameParams {
                highlight_segment: -1,
                pointer_angle_deg: angle,
                fate_number,
                reveal_number: false,
                clear_background: false,
                draw_wheel: false,
            },
        );
        flush_and_sleep(ctx, SPIN_FRAME_DELAY_MS as u32);
    }

    // Phase 3: Land on target segment
    let final_angle = start_angle + total_rotation;
    render_wheel_frame(
        ctx,
        width,
        height,
        center_x,
        center_y,
        &layout,
        &mut last_pointer_angle,
        &FrameParams {
            highlight_segment: target_segment,
            pointer_angle_deg: final_angle,
            fate_number,
            reveal_number: false,
            clear_background: true,
            draw_wheel: true,
        },
    );
    flush_and_sleep(ctx, 900);

    // Phase 4: Flash the landing segment
    for flash in 0..5 {
        render_wheel_frame(
            ctx,
            width,
            height,
            center_x,
            center_y,
            &layout,
            &mut last_pointer_angle,
            &FrameParams {
                highlight_segment: target_segment,
                pointer_angle_deg: final_angle,
                fate_number,
                reveal_number: true,
                clear_background: false,
                draw_wheel: false,
            },
        );
        flush_and_sleep(ctx, 250);
        if flash < 4 {
            render_wheel_frame(
                ctx,
                width,
                height,
                center_x,
                center_y,
                &layout,
                &mut last_pointer_angle,
                &FrameParams {
                    highlight_segment: target_segment,
                    pointer_angle_deg: final_angle,
                    fate_number,
                    reveal_number: false,
                    clear_background: false,
                    draw_wheel: false,
                },
            );
            flush_and_sleep(ctx, 150);
        }
    }

    // Phase 5: Final reveal
    render_wheel_frame(
        ctx,
        width,
        height,
        center_x,
        center_y,
        &layout,
        &mut last_pointer_angle,
        &FrameParams {
            highlight_segment: target_segment,
            pointer_angle_deg: final_angle,
            fate_number,
            reveal_number: true,
            clear_background: false,
            draw_wheel: true,
        },
    );
    flush_and_sleep(ctx, 600);

    // Phase 6: Result banner
    let fate_box_y = center_y + layout.radius + 22;
    let fate_box_h = 88;
    let info_y = (fate_box_y + fate_box_h + 10).clamp(0, height);
    canvas_ops::fill_rect(ctx, 0, info_y, width, height - info_y, BG_COLOR);

    let banner_y = info_y + 12;
    draw_result_banner(ctx, center_x, banner_y, fate_number);
    flush_and_sleep(ctx, RESULT_DELAY_MS);

    // Phase 7: Transition spinner
    draw_transition_spinner(ctx, width, height, (fate_number & 1) == 1);

    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn roulette_draw_kernel(fate_number: u32) -> VideoResult {
    let mut ctx = GraphicsContext::new()?;
    run_animation(&mut ctx, fate_number)
}
