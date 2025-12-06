/*
 * SlopOS Kernel Roulette Visual Screen Implementation
 * The Wheel of Fate - Now with 100% more visual gambling addiction
 */

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include "roulette.h"
#include "framebuffer.h"
#include "graphics.h"
#include "font.h"
#include "splash.h"
#include "../drivers/serial.h"
#include "../drivers/pit.h"
#include "../lib/numfmt.h"
#include "../boot/kernel_panic.h"
#include "../lib/klog.h"

/* ========================================================================
 * GEOMETRY DEFINITIONS
 * ======================================================================== */

#define ROULETTE_SEGMENT_COUNT            12
#define ROULETTE_TRIG_SCALE             1024
#define ROULETTE_WHEEL_RADIUS            120
#define ROULETTE_INNER_RADIUS             36
#define ROULETTE_POINTER_WIDTH            18
#define ROULETTE_DEGREE_STEPS            360
#define ROULETTE_SEGMENT_DEGREES         (360 / ROULETTE_SEGMENT_COUNT)
#define ROULETTE_SPIN_LOOPS                4
#define ROULETTE_SPIN_DURATION_MS       4200
#define ROULETTE_SPIN_FRAME_DELAY_MS      16

/* Alternating colored vs “blank” wedges */
struct roulette_segment_def {
    bool is_colored;
};

static const struct roulette_segment_def roulette_segments[ROULETTE_SEGMENT_COUNT] = {
    { true }, { false }, { true }, { false }, { true }, { false },
    { true }, { false }, { true }, { false }, { true }, { false },
};

static const int16_t roulette_cos_table[ROULETTE_SEGMENT_COUNT + 1] = {
    1024,  887,  512,    0,  -512, -887, -1024, -887, -512,    0,  512,  887, 1024
};

static const int16_t roulette_sin_table[ROULETTE_SEGMENT_COUNT + 1] = {
       0,  512,  887, 1024,   887,  512,     0, -512, -887, -1024, -887, -512,    0
};

/* Midpoint directions (15, 45, ... degrees) for pointer ticks */
static const int16_t roulette_cos360[ROULETTE_DEGREE_STEPS] = {
    1024, 1024, 1023, 1023, 1022, 1020, 1018, 1016, 1014, 1011, 1008, 1005,
    1002,  998,  994,  989,  984,  979,  974,  968,  962,  956,  949,  943,
     935,  928,  920,  912,  904,  896,  887,  878,  868,  859,  849,  839,
     828,  818,  807,  796,  784,  773,  761,  749,  737,  724,  711,  698,
     685,  672,  658,  644,  630,  616,  602,  587,  573,  558,  543,  527,
     512,  496,  481,  465,  449,  433,  416,  400,  384,  367,  350,  333,
     316,  299,  282,  265,  248,  230,  213,  195,  178,  160,  143,  125,
     107,   89,   71,   54,   36,   18,    0,  -18,  -36,  -54,  -71,  -89,
    -107, -125, -143, -160, -178, -195, -213, -230, -248, -265, -282, -299,
    -316, -333, -350, -367, -384, -400, -416, -433, -449, -465, -481, -496,
    -512, -527, -543, -558, -573, -587, -602, -616, -630, -644, -658, -672,
    -685, -698, -711, -724, -737, -749, -761, -773, -784, -796, -807, -818,
    -828, -839, -849, -859, -868, -878, -887, -896, -904, -912, -920, -928,
    -935, -943, -949, -956, -962, -968, -974, -979, -984, -989, -994, -998,
   -1002, -1005, -1008, -1011, -1014, -1016, -1018, -1020, -1022, -1023, -1023,
   -1024, -1024, -1024, -1023, -1023, -1022, -1020, -1018, -1016, -1014, -1011,
   -1008, -1005, -1002,  -998,  -994,  -989,  -984,  -979,  -974,  -968,  -962,
    -956,  -949,  -943,  -935,  -928,  -920,  -912,  -904,  -896,  -887,  -878,
    -868,  -859,  -849,  -839,  -828,  -818,  -807,  -796,  -784,  -773,  -761,
    -749,  -737,  -724,  -711,  -698,  -685,  -672,  -658,  -644,  -630,  -616,
    -602,  -587,  -573,  -558,  -543,  -527,  -512,  -496,  -481,  -465,  -449,
    -433,  -416,  -400,  -384,  -367,  -350,  -333,  -316,  -299,  -282,  -265,
    -248,  -230,  -213,  -195,  -178,  -160,  -143,  -125,  -107,   -89,   -71,
     -54,   -36,   -18,     0,    18,    36,    54,    71,    89,   107,   125,
     143,   160,   178,   195,   213,   230,   248,   265,   282,   299,   316,
     333,   350,   367,   384,   400,   416,   433,   449,   465,   481,   496,
     512,   527,   543,   558,   573,   587,   602,   616,   630,   644,   658,
     672,   685,   698,   711,   724,   737,   749,   761,   773,   784,   796,
     807,   818,   828,   839,   849,   859,   868,   878,   887,   896,   904,
     912,   920,   928,   935,   943,   949,   956,   962,   968,   974,   979,
     984,   989,   994,   998,  1002,  1005,  1008,  1011,  1014,  1016,  1018,
    1020,  1022,  1023,  1023,  1024
};

static const int16_t roulette_sin360[ROULETTE_DEGREE_STEPS] = {
       0,   18,   36,   54,   71,   89,  107,  125,  143,  160,  178,  195,
     213,  230,  248,  265,  282,  299,  316,  333,  350,  367,  384,  400,
     416,  433,  449,  465,  481,  496,  512,  527,  543,  558,  573,  587,
     602,  616,  630,  644,  658,  672,  685,  698,  711,  724,  737,  749,
     761,  773,  784,  796,  807,  818,  828,  839,  849,  859,  868,  878,
     887,  896,  904,  912,  920,  928,  935,  943,  949,  956,  962,  968,
     974,  979,  984,  989,  994,  998, 1002, 1005, 1008, 1011, 1014, 1016,
    1018, 1020, 1022, 1023, 1023, 1024, 1024, 1024, 1023, 1023, 1022, 1020,
    1018, 1016, 1014, 1011, 1008, 1005, 1002,  998,  994,  989,  984,  979,
     974,  968,  962,  956,  949,  943,  935,  928,  920,  912,  904,  896,
     887,  878,  868,  859,  849,  839,  828,  818,  807,  796,  784,  773,
     761,  749,  737,  724,  711,  698,  685,  672,  658,  644,  630,  616,
     602,  587,  573,  558,  543,  527,  512,  496,  481,  465,  449,  433,
     416,  400,  384,  367,  350,  333,  316,  299,  282,  265,  248,  230,
     213,  195,  178,  160,  143,  125,  107,   89,   71,   54,   36,   18,
       0,  -18,  -36,  -54,  -71,  -89, -107, -125, -143, -160, -178, -195,
    -213, -230, -248, -265, -282, -299, -316, -333, -350, -367, -384, -400,
    -416, -433, -449, -465, -481, -496, -512, -527, -543, -558, -573, -587,
    -602, -616, -630, -644, -658, -672, -685, -698, -711, -724, -737, -749,
    -761, -773, -784, -796, -807, -818, -828, -839, -849, -859, -868, -878,
    -887, -896, -904, -912, -920, -928, -935, -943, -949, -956, -962, -968,
    -974, -979, -984, -989, -994, -998, -1002, -1005, -1008, -1011, -1014,
   -1016, -1018, -1020, -1022, -1023, -1023, -1024, -1024, -1024, -1023, -1023,
   -1022, -1020, -1018, -1016, -1014, -1011, -1008, -1005, -1002,  -998,  -994,
    -989,  -984,  -979,  -974,  -968,  -962,  -956,  -949,  -943,  -935,  -928,
    -920,  -912,  -904,  -896,  -887,  -878,  -868,  -859,  -849,  -839,  -828,
    -818,  -807,  -796,  -784,  -773,  -761,  -749,  -737,  -724,  -711,  -698,
    -685,  -672,  -658,  -644,  -630,  -616,  -602,  -587,  -573,  -558,  -543,
    -527,  -512,  -496,  -481,  -465,  -449,  -433,  -416,  -400,  -384,  -367,
    -350,  -333,  -316,  -299,  -282,  -265,  -248,  -230,  -213,  -195,  -178,
    -160,  -143,  -125,  -107,   -89,   -71,   -54,   -36,   -18
};

static const uint32_t ROULETTE_BLANK_COLOR = 0x181818FF;
static const uint32_t ROULETTE_BLANK_HIGHLIGHT = 0x444444FF;
static const uint32_t ROULETTE_COLORED_HIGHLIGHT = 0x66FF66FF;
static const uint32_t ROULETTE_POINTER_COLOR = 0xFFFF00FF;

static inline int roulette_normalize_angle(int degrees) {
    int angle = degrees % ROULETTE_DEGREE_STEPS;
    if (angle < 0) {
        angle += ROULETTE_DEGREE_STEPS;
    }
    return angle;
}

static inline int16_t roulette_cos_deg(int degrees) {
    return roulette_cos360[roulette_normalize_angle(degrees)];
}

static inline int16_t roulette_sin_deg(int degrees) {
    return roulette_sin360[roulette_normalize_angle(degrees)];
}

static inline int segment_center_angle(int segment_index) {
    return segment_index * ROULETTE_SEGMENT_DEGREES + (ROULETTE_SEGMENT_DEGREES / 2);
}

static void draw_fate_number(int center_x, int y_pos, uint32_t fate_number, int revealed);

/* ========================================================================
 * WHEEL DRAWING FUNCTIONS
 * ======================================================================== */

static inline int roulette_scale(int16_t value, int radius) {
    return (value * radius) / ROULETTE_TRIG_SCALE;
}

static void draw_segment_wedge(int center_x, int center_y, int start_idx,
                               int radius, uint32_t color) {
    int inner = ROULETTE_INNER_RADIUS;
    int16_t start_cos = roulette_cos_table[start_idx];
    int16_t start_sin = roulette_sin_table[start_idx];
    int16_t end_cos = roulette_cos_table[start_idx + 1];
    int16_t end_sin = roulette_sin_table[start_idx + 1];

    for (int r = inner; r <= radius; r++) {
        int x1 = center_x + roulette_scale(start_cos, r);
        int y1 = center_y + roulette_scale(start_sin, r);
        int x2 = center_x + roulette_scale(end_cos, r);
        int y2 = center_y + roulette_scale(end_sin, r);
        graphics_draw_line(x1, y1, x2, y2, color);
    }
}

static void draw_segment_divider(int center_x, int center_y, int idx, int radius) {
    int x_outer = center_x + roulette_scale(roulette_cos_table[idx], radius + 2);
    int y_outer = center_y + roulette_scale(roulette_sin_table[idx], radius + 2);
    graphics_draw_line(center_x, center_y, x_outer, y_outer, ROULETTE_WHEEL_COLOR);
}

/*
 * Draw a roulette wheel with alternating colored/blank wedges.
 * highlight_segment >= 0 draws a glow under the pointer location.
 */
static void draw_roulette_wheel(int center_x, int center_y, int radius, int highlight_segment) {
    // Outer ring
    graphics_draw_circle_filled(center_x, center_y, radius + 8, 0x000000FF);
    graphics_draw_circle(center_x, center_y, radius + 8, ROULETTE_WHEEL_COLOR);

    for (int i = 0; i < ROULETTE_SEGMENT_COUNT; i++) {
        bool is_colored = roulette_segments[i].is_colored;
        uint32_t base_color = is_colored ? ROULETTE_ODD_COLOR : ROULETTE_BLANK_COLOR;
        if (i == highlight_segment) {
            base_color = is_colored ? ROULETTE_COLORED_HIGHLIGHT : ROULETTE_BLANK_HIGHLIGHT;
        }
        draw_segment_wedge(center_x, center_y, i, radius, base_color);
        draw_segment_divider(center_x, center_y, i, radius);
    }
    draw_segment_divider(center_x, center_y, ROULETTE_SEGMENT_COUNT, radius);

    // Center hub
    graphics_draw_circle_filled(center_x, center_y, ROULETTE_INNER_RADIUS + 6, ROULETTE_WHEEL_COLOR);
    graphics_draw_circle_filled(center_x, center_y, ROULETTE_INNER_RADIUS, 0x000000FF);
}

static void draw_pointer_for_angle(int center_x, int center_y, int radius,
                                   int angle_deg, uint32_t color) {
    int16_t dir_x = roulette_cos_deg(angle_deg);
    int16_t dir_y = roulette_sin_deg(angle_deg);
    int16_t perp_x = -dir_y;
    int16_t perp_y = dir_x;

    int tip_radius = radius + 36;
    int base_radius = radius - 6;

    int tip_x = center_x + roulette_scale(dir_x, tip_radius);
    int tip_y = center_y + roulette_scale(dir_y, tip_radius);
    int base_x = center_x + roulette_scale(dir_x, base_radius);
    int base_y = center_y + roulette_scale(dir_y, base_radius);

    int offset_x = roulette_scale(perp_x, ROULETTE_POINTER_WIDTH);
    int offset_y = roulette_scale(perp_y, ROULETTE_POINTER_WIDTH);

    int left_x = base_x + offset_x;
    int left_y = base_y + offset_y;
    int right_x = base_x - offset_x;
    int right_y = base_y - offset_y;

    graphics_draw_line(tip_x, tip_y, left_x, left_y, color);
    graphics_draw_line(tip_x, tip_y, right_x, right_y, color);
    graphics_draw_line(left_x, left_y, right_x, right_y, color);
}

static void draw_pointer_ticks(int center_x, int center_y, int radius,
                               int angle_deg, uint32_t color) {
    draw_pointer_for_angle(center_x, center_y, radius, angle_deg, color);
    draw_pointer_for_angle(center_x, center_y, radius, angle_deg + 180, color);
}

static void render_wheel_frame(int screen_width, int screen_height,
                               int center_x, int center_y, int radius,
                               int highlight_segment, int pointer_angle_deg,
                               int *last_pointer_angle,
                               uint32_t fate_number, bool reveal_number,
                               bool clear_background) {
    int region = radius + 80;
    int region_x = center_x - region;
    int region_y = center_y - region;
    int region_w = region * 2;
    int region_h = region * 2;

    if (!clear_background && last_pointer_angle && *last_pointer_angle >= 0) {
        draw_pointer_ticks(center_x, center_y, radius, *last_pointer_angle, ROULETTE_BG_COLOR);
    }

    if (clear_background) {
        if (region_x < 0) {
            region_w += region_x;
            region_x = 0;
        }
        if (region_y < 0) {
            region_h += region_y;
            region_y = 0;
        }
        if (region_x + region_w > screen_width) {
            region_w = screen_width - region_x;
        }
        if (region_y + region_h > screen_height) {
            region_h = screen_height - region_y;
        }

        graphics_draw_rect_filled_fast(region_x, region_y, region_w, region_h, ROULETTE_BG_COLOR);
    }

    draw_roulette_wheel(center_x, center_y, radius, highlight_segment);
    draw_pointer_ticks(center_x, center_y, radius, pointer_angle_deg, ROULETTE_POINTER_COLOR);
    draw_fate_number(center_x, center_y + radius + 30, fate_number, reveal_number ? 1 : 0);

    if (last_pointer_angle) {
        *last_pointer_angle = pointer_angle_deg;
    }
}

static bool segment_matches_parity(int segment_index, bool need_colored) {
    bool is_colored = roulette_segments[segment_index % ROULETTE_SEGMENT_COUNT].is_colored;
    return need_colored ? is_colored : !is_colored;
}

static int choose_segment_for_parity(uint32_t fate_number, bool need_colored) {
    int start = fate_number % ROULETTE_SEGMENT_COUNT;
    for (int tries = 0; tries < ROULETTE_SEGMENT_COUNT; tries++) {
        int idx = (start + tries) % ROULETTE_SEGMENT_COUNT;
        if (segment_matches_parity(idx, need_colored)) {
            return idx;
        }
    }
    return start;
}

/*
 * Draw the fate number display
 */
static void draw_fate_number(int center_x, int y_pos, uint32_t fate_number, int revealed) {
    if (!revealed) {
        // Draw mystery box
        graphics_draw_rect_filled(center_x - 100, y_pos, 200, 60, 0x333333FF);
        graphics_draw_rect(center_x - 100, y_pos, 200, 60, ROULETTE_WHEEL_COLOR);
        font_draw_string(center_x - 40, y_pos + 20, "? ? ?", ROULETTE_TEXT_COLOR, 0x00000000);
    } else {
        // Draw number box
        uint32_t box_color = (fate_number & 1) ? ROULETTE_ODD_COLOR : ROULETTE_EVEN_COLOR;
        graphics_draw_rect_filled(center_x - 100, y_pos, 200, 60, box_color);
        graphics_draw_rect(center_x - 100, y_pos, 200, 60, ROULETTE_WHEEL_COLOR);

        // Convert fate number to displayable string
        char num_str[21];
        size_t len = numfmt_u64_to_decimal((uint64_t)fate_number, num_str, sizeof(num_str));
        if (len == 0) {
            num_str[0] = '0';
            num_str[1] = '\0';
            len = 1;
        }

        // Center the number
        int text_x = center_x - ((int)len * 8) / 2;
        font_draw_string(text_x, y_pos + 20, num_str, 0x000000FF, 0x00000000);
    }
}

/*
 * Draw result banner (WIN or LOSE)
 */
static void draw_result_banner(int center_x, int y_pos, uint32_t fate_number) {
    const char *result_text;
    const char *sub_text;
    uint32_t banner_color;

    if (fate_number & 1) {
        // ODD = WIN
        result_text = "W I N !";
        sub_text = "Fortune smiles upon the slop!";
        banner_color = ROULETTE_WIN_COLOR;
    } else {
        // EVEN = LOSE
        result_text = "L O S E";
        sub_text = "L bozzo lol - try again!";
        banner_color = ROULETTE_LOSE_COLOR;
    }

    // Draw result banner
    graphics_draw_rect_filled(center_x - 200, y_pos, 400, 80, banner_color);
    graphics_draw_rect(center_x - 202, y_pos - 2, 404, 84, ROULETTE_WHEEL_COLOR);

    // Draw text (centered)
    int text_x = center_x - 60;
    font_draw_string(text_x, y_pos + 15, result_text, 0x000000FF, 0x00000000);

    // Draw subtext
    int sub_x = center_x - 140;
    font_draw_string(sub_x, y_pos + 50, sub_text, 0x000000FF, 0x00000000);
}

/* ========================================================================
 * MAIN ROULETTE SCREEN FUNCTION
 * ======================================================================== */

/*
 * Show the full roulette spinning animation and result
 */
int roulette_show_spin(uint32_t fate_number) {
    if (!framebuffer_is_initialized()) {
        klog(KLOG_INFO, "ROULETTE: Framebuffer not available, using fallback");
        roulette_show_spin_fallback(fate_number);
        return -1;
    }

    klog(KLOG_INFO, "ROULETTE: Displaying visual wheel of fate...");

    uint32_t width = framebuffer_get_width();
    uint32_t height = framebuffer_get_height();
    
    if (width == 0 || height == 0) {
        kernel_panic("ROULETTE: Invalid framebuffer dimensions");
    }

    int center_x = width / 2;
    int center_y = height / 2;

    if (graphics_draw_rect_filled_fast(0, 0, width, height, ROULETTE_BG_COLOR) != 0) {
        kernel_panic("ROULETTE: Failed to clear screen");
    }

    font_draw_string(center_x - 150, 50, "=== THE WHEEL OF FATE ===", ROULETTE_WHEEL_COLOR, 0x00000000);
    font_draw_string(center_x - 120, 80, "Pointers choose your destiny...", ROULETTE_TEXT_COLOR, 0x00000000);

    int radius = ROULETTE_WHEEL_RADIUS;
    int max_radius = ((width < height ? width : height) / 2) - 60;
    if (radius > max_radius) {
        radius = max_radius;
    }
    if (radius < ROULETTE_INNER_RADIUS + 20) {
        radius = ROULETTE_INNER_RADIUS + 20;
    }

    bool want_colored = (fate_number & 1) != 0;
    int start_segment = fate_number % ROULETTE_SEGMENT_COUNT;
    int target_segment = choose_segment_for_parity(fate_number, want_colored);
    if (start_segment == target_segment) {
        start_segment = (start_segment + 3) % ROULETTE_SEGMENT_COUNT;
    }

    pit_sleep_ms(300);

    int start_angle = segment_center_angle(start_segment);
    int target_angle = segment_center_angle(target_segment);
    int rotation_to_target = roulette_normalize_angle(target_angle - start_angle);
    int total_rotation = ROULETTE_SPIN_LOOPS * ROULETTE_DEGREE_STEPS + rotation_to_target;
    if (total_rotation <= 0) {
        total_rotation += ROULETTE_DEGREE_STEPS;
    }

    int last_pointer_angle = -1;
    render_wheel_frame(width, height, center_x, center_y, radius,
                       -1, start_angle, &last_pointer_angle, fate_number, false, true);

    int total_frames = ROULETTE_SPIN_DURATION_MS / ROULETTE_SPIN_FRAME_DELAY_MS;
    if (total_frames < 1) {
        total_frames = 1;
    }

    klog(KLOG_INFO, "ROULETTE: Animating pointer sweep");
    for (int frame = 1; frame <= total_frames; frame++) {
        int pointer_angle_frame = start_angle + (total_rotation * frame) / total_frames;
        render_wheel_frame(width, height, center_x, center_y, radius,
                           -1, pointer_angle_frame, &last_pointer_angle,
                           fate_number, false, false);
        pit_sleep_ms(ROULETTE_SPIN_FRAME_DELAY_MS);
    }

    int pointer_angle = start_angle + total_rotation;
    int landing_segment = target_segment;
    render_wheel_frame(width, height, center_x, center_y, radius,
                       landing_segment, pointer_angle, &last_pointer_angle,
                       fate_number, false, true);
    pit_sleep_ms(500);

    klog(KLOG_INFO, "ROULETTE: Revealing fate number...");
    pit_sleep_ms(400);

    for (int flash = 0; flash < 5; flash++) {
        render_wheel_frame(width, height, center_x, center_y, radius,
                           landing_segment, pointer_angle, &last_pointer_angle,
                           fate_number, true, false);
        pit_sleep_ms(250);
        if (flash < 4) {
            render_wheel_frame(width, height, center_x, center_y, radius,
                               landing_segment, pointer_angle, &last_pointer_angle,
                               fate_number, false, false);
            pit_sleep_ms(150);
        }
    }
    render_wheel_frame(width, height, center_x, center_y, radius,
                       landing_segment, pointer_angle, &last_pointer_angle,
                       fate_number, true, false);
    pit_sleep_ms(600);

    klog(KLOG_INFO, "ROULETTE: Displaying result...");
    int info_y = center_y + radius + 60;
    if (info_y < 0) {
        info_y = 0;
    }
    if (info_y > (int)height) {
        info_y = height;
    }
    graphics_draw_rect_filled_fast(0, info_y, width, height - info_y, ROULETTE_BG_COLOR);
    draw_result_banner(center_x, center_y + radius + 80, fate_number);

    const char *currency_text = (fate_number & 1) ? "+10 W's (currency units)" : "-10 W's (currency units)";
    font_draw_string(center_x - 110, center_y + radius + 170, currency_text, ROULETTE_TEXT_COLOR, 0x00000000);

    if ((fate_number & 1) == 0) {
        font_draw_string(center_x - 130, center_y + radius + 210, "Press RESET to try again...", 0xFFFF00FF, 0x00000000);
    } else {
        font_draw_string(center_x - 130, center_y + radius + 210, "Continuing to OS...", 0x00FF00FF, 0x00000000);
    }

    pit_sleep_ms(ROULETTE_RESULT_DELAY_MS);

    klog(KLOG_INFO, "ROULETTE: Wheel of fate complete");

    if (fate_number & 1) {
        graphics_draw_rect_filled_fast(0, 0, width, height, 0x001122FF);
        uint32_t cur_width = framebuffer_get_width();
        uint32_t cur_height = framebuffer_get_height();
        int msg_x = cur_width / 2 - 150;
        int msg_y = cur_height / 2 - 20;

        font_draw_string(msg_x, msg_y, "You won! Continuing to SlopOS...", 0xFFFFFFFF, 0x00000000);
        pit_sleep_ms(1000);
        splash_draw_graphics_demo();
        klog(KLOG_INFO, "ROULETTE: Graphics demo restored, returning to OS");
    }

    return want_colored ? 0 : 1;
}

/*
 * Fallback roulette display for when framebuffer is not available
 */
void roulette_show_spin_fallback(uint32_t fate_number) {
    klog(KLOG_INFO, "ROULETTE: Using text-only fallback display");
    klog(KLOG_INFO, "");
    klog(KLOG_INFO, "========================================");
    klog(KLOG_INFO, "    THE WHEEL OF FATE IS SPINNING     ");
    klog(KLOG_INFO, "========================================");
    klog(KLOG_INFO, "");

    // Simple text animation
    for (int i = 0; i < 5; i++) {
        klog_raw(KLOG_INFO, ".");
        pit_sleep_ms(200);
    }
    klog(KLOG_INFO, "");

    klog(KLOG_INFO, "");
    klog_raw(KLOG_INFO, "Fate number: ");
    klog_decimal(KLOG_INFO, fate_number);
    klog(KLOG_INFO, "");

    if (fate_number & 1) {
        klog(KLOG_INFO, "");
        klog(KLOG_INFO, "========================================");
        klog(KLOG_INFO, "           W I N !                      ");
        klog(KLOG_INFO, "    Fortune smiles upon the slop!      ");
        klog(KLOG_INFO, "========================================");
    } else {
        klog(KLOG_INFO, "");
        klog(KLOG_INFO, "========================================");
        klog(KLOG_INFO, "           L O S E                      ");
        klog(KLOG_INFO, "      L bozzo lol - try again!         ");
        klog(KLOG_INFO, "========================================");
    }

    klog(KLOG_INFO, "");
    pit_sleep_ms(1000);
}
