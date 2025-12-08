/*
 * Shared roulette rendering core.
 * This mirrors the original kernel roulette visuals but delegates drawing and
 * timing to a backend so it can run in both kernel and user space.
 */

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include "roulette.h"
#include "roulette_core.h"

#define ROULETTE_BLANK_COLOR       0x181818FF
#define ROULETTE_BLANK_HIGHLIGHT   0x444444FF
#define ROULETTE_COLORED_HIGHLIGHT 0x66FF66FF
#define ROULETTE_POINTER_COLOR     0xFFFF00FF

#define ROULETTE_SEGMENT_COUNT            12
#define ROULETTE_TRIG_SCALE             1024
#define ROULETTE_WHEEL_RADIUS            120
#define ROULETTE_INNER_RADIUS             36
#define ROULETTE_POINTER_WIDTH            18
#define ROULETTE_DEGREE_STEPS            360
#define ROULETTE_SEGMENT_DEGREES         (360 / ROULETTE_SEGMENT_COUNT)
#define ROULETTE_SPIN_LOOPS                4
#define ROULETTE_SPIN_DURATION_MS       3600
#define ROULETTE_SPIN_FRAME_DELAY_MS      12

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

static inline int roulette_scale(int16_t value, int radius) {
    return (value * radius) / ROULETTE_TRIG_SCALE;
}

static void draw_segment_wedge(const struct roulette_backend *b, int cx, int cy, int start_idx,
                               int radius, uint32_t color) {
    int inner = ROULETTE_INNER_RADIUS;
    int16_t start_cos = roulette_cos_table[start_idx];
    int16_t start_sin = roulette_sin_table[start_idx];
    int16_t end_cos = roulette_cos_table[start_idx + 1];
    int16_t end_sin = roulette_sin_table[start_idx + 1];

    for (int r = inner; r <= radius; r++) {
        int x1 = cx + roulette_scale(start_cos, r);
        int y1 = cy + roulette_scale(start_sin, r);
        int x2 = cx + roulette_scale(end_cos, r);
        int y2 = cy + roulette_scale(end_sin, r);
        b->draw_line(b->ctx, x1, y1, x2, y2, color);
    }
}

static void draw_segment_divider(const struct roulette_backend *b, int cx, int cy, int idx, int radius) {
    int x_outer = cx + roulette_scale(roulette_cos_table[idx], radius + 2);
    int y_outer = cy + roulette_scale(roulette_sin_table[idx], radius + 2);
    b->draw_line(b->ctx, cx, cy, x_outer, y_outer, ROULETTE_WHEEL_COLOR);
}

static void draw_roulette_wheel(const struct roulette_backend *b, int cx, int cy, int radius, int highlight_segment) {
    /* Outer ring */
    b->draw_circle_filled(b->ctx, cx, cy, radius + 8, 0x000000FF);
    b->draw_circle(b->ctx, cx, cy, radius + 8, ROULETTE_WHEEL_COLOR);

    for (int i = 0; i < ROULETTE_SEGMENT_COUNT; i++) {
        bool is_colored = roulette_segments[i].is_colored;
        uint32_t base_color = is_colored ? ROULETTE_ODD_COLOR : ROULETTE_BLANK_COLOR;
        if (i == highlight_segment) {
            base_color = is_colored ? ROULETTE_COLORED_HIGHLIGHT : ROULETTE_BLANK_HIGHLIGHT;
        }
        draw_segment_wedge(b, cx, cy, i, radius, base_color);
        draw_segment_divider(b, cx, cy, i, radius);
    }
    draw_segment_divider(b, cx, cy, ROULETTE_SEGMENT_COUNT, radius);

    /* Center hub */
    b->draw_circle_filled(b->ctx, cx, cy, ROULETTE_INNER_RADIUS + 6, ROULETTE_WHEEL_COLOR);
    b->draw_circle_filled(b->ctx, cx, cy, ROULETTE_INNER_RADIUS, 0x000000FF);
}

static void draw_pointer_for_angle(const struct roulette_backend *b, int cx, int cy, int radius,
                                   int angle_deg, uint32_t color) {
    int16_t dir_x = roulette_cos_deg(angle_deg);
    int16_t dir_y = roulette_sin_deg(angle_deg);
    int16_t perp_x = -dir_y;
    int16_t perp_y = dir_x;

    int tip_radius = radius + 36;
    int base_radius = radius - 6;

    int tip_x = cx + roulette_scale(dir_x, tip_radius);
    int tip_y = cy + roulette_scale(dir_y, tip_radius);
    int base_x = cx + roulette_scale(dir_x, base_radius);
    int base_y = cy + roulette_scale(dir_y, base_radius);

    int offset_x = roulette_scale(perp_x, ROULETTE_POINTER_WIDTH);
    int offset_y = roulette_scale(perp_y, ROULETTE_POINTER_WIDTH);

    int left_x = base_x + offset_x;
    int left_y = base_y + offset_y;
    int right_x = base_x - offset_x;
    int right_y = base_y - offset_y;

    b->draw_line(b->ctx, tip_x, tip_y, left_x, left_y, color);
    b->draw_line(b->ctx, tip_x, tip_y, right_x, right_y, color);
    b->draw_line(b->ctx, left_x, left_y, right_x, right_y, color);
}

static void draw_pointer_ticks(const struct roulette_backend *b, int cx, int cy, int radius,
                               int angle_deg, uint32_t color) {
    draw_pointer_for_angle(b, cx, cy, radius, angle_deg, color);
    draw_pointer_for_angle(b, cx, cy, radius, angle_deg + 180, color);
}

static void draw_fate_number(const struct roulette_backend *b, int cx, int y_pos, uint32_t fate_number, int revealed) {
    char num_str[21];

    if (!revealed) {
        b->fill_rect(b->ctx, cx - 100, y_pos, 200, 60, 0x333333FF);
        b->draw_line(b->ctx, cx - 100, y_pos, cx + 100, y_pos, ROULETTE_WHEEL_COLOR);
        b->draw_line(b->ctx, cx - 100, y_pos + 60, cx + 100, y_pos + 60, ROULETTE_WHEEL_COLOR);
        b->draw_text(b->ctx, cx - 40, y_pos + 20, "? ? ?", ROULETTE_TEXT_COLOR, 0x00000000);
        return;
    }

    uint32_t box_color = (fate_number & 1) ? ROULETTE_ODD_COLOR : ROULETTE_EVEN_COLOR;
    b->fill_rect(b->ctx, cx - 100, y_pos, 200, 60, box_color);
    b->draw_line(b->ctx, cx - 100, y_pos, cx + 100, y_pos, ROULETTE_WHEEL_COLOR);
    b->draw_line(b->ctx, cx - 100, y_pos + 60, cx + 100, y_pos + 60, ROULETTE_WHEEL_COLOR);

    /* decimal conversion */
    uint64_t n = fate_number;
    size_t len = 0;
    if (n == 0) {
        num_str[len++] = '0';
    } else {
        char tmp[21];
        size_t t = 0;
        while (n && t < sizeof(tmp)) {
            tmp[t++] = (char)('0' + (n % 10));
            n /= 10;
        }
        while (t > 0) {
            num_str[len++] = tmp[--t];
        }
    }
    num_str[len] = '\0';

    int text_x = cx - ((int)len * 8) / 2;
    b->draw_text(b->ctx, text_x, y_pos + 20, num_str, 0x000000FF, 0x00000000);
}

static void draw_result_banner(const struct roulette_backend *b, int cx, int y_pos, uint32_t fate_number) {
    const char *result_text;
    const char *sub_text;
    uint32_t banner_color;

    if (fate_number & 1) {
        result_text = "W I N !";
        sub_text = "Fortune smiles upon the slop!";
        banner_color = ROULETTE_WIN_COLOR;
    } else {
        result_text = "L O S E";
        sub_text = "L bozzo lol - try again!";
        banner_color = ROULETTE_LOSE_COLOR;
    }

    b->fill_rect(b->ctx, cx - 200, y_pos, 400, 80, banner_color);
    b->draw_line(b->ctx, cx - 202, y_pos - 2, cx + 202, y_pos - 2, ROULETTE_WHEEL_COLOR);
    b->draw_line(b->ctx, cx - 202, y_pos + 82, cx + 202, y_pos + 82, ROULETTE_WHEEL_COLOR);

    b->draw_text(b->ctx, cx - 60, y_pos + 15, result_text, 0x000000FF, 0x00000000);
    b->draw_text(b->ctx, cx - 140, y_pos + 50, sub_text, 0x000000FF, 0x00000000);
}

static void render_wheel_frame(const struct roulette_backend *b,
                               int screen_width, int screen_height,
                               int cx, int cy, int radius,
                               int highlight_segment, int pointer_angle_deg,
                               int *last_pointer_angle,
                               uint32_t fate_number, bool reveal_number,
                               bool clear_background, bool draw_wheel) {
    int region = radius + 80;
    int region_x = cx - region;
    int region_y = cy - region;
    int region_w = region * 2;
    int region_h = region * 2;

    if (!clear_background && last_pointer_angle && *last_pointer_angle >= 0) {
        draw_pointer_ticks(b, cx, cy, radius, *last_pointer_angle, ROULETTE_BG_COLOR);
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

        b->fill_rect(b->ctx, region_x, region_y, region_w, region_h, ROULETTE_BG_COLOR);
    }

    if (draw_wheel) {
        draw_roulette_wheel(b, cx, cy, radius, highlight_segment);
    }
    draw_pointer_ticks(b, cx, cy, radius, pointer_angle_deg, ROULETTE_POINTER_COLOR);
    draw_fate_number(b, cx, cy + radius + 30, fate_number, reveal_number ? 1 : 0);

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

static void roulette_draw_demo_scene(const struct roulette_backend *b, int width, int height) {
    /* Recreate the boot demo (rectangles, circle, border, text) from user mode. */
    b->fill_rect(b->ctx, 0, 0, width, height, 0x001122FF);

    /* Colored rectangles */
    b->fill_rect(b->ctx, 20, 20, 300, 150, 0xFF0000FF);   /* Red */
    b->fill_rect(b->ctx, width - 320, 20, 300, 150, 0x00FF00FF); /* Green */

    /* Yellow circle roughly centered */
    int cx = width / 2;
    int cy = height / 2;
    int radius = (width < height ? width : height) / 8;
    if (radius < 60) radius = 60;
    b->draw_circle(b->ctx, cx, cy, radius, 0xFFFF00FF);

    /* White border */
    b->fill_rect(b->ctx, 0, 0, width, 4, 0xFFFFFFFF);
    b->fill_rect(b->ctx, 0, height - 4, width, 4, 0xFFFFFFFF);
    b->fill_rect(b->ctx, 0, 0, 4, height, 0xFFFFFFFF);
    b->fill_rect(b->ctx, width - 4, 0, 4, height, 0xFFFFFFFF);

    /* Text lines */
    b->draw_text(b->ctx, 20, height - 140, "*** SLOPOS GRAPHICS SYSTEM OPERATIONAL ***", 0xFFFFFFFF, 0x00000000);
    b->draw_text(b->ctx, 20, height - 124, "Framebuffer: WORKING | Resolution: 1024x768", 0xFFFFFFFF, 0x00000000);
    b->draw_text(b->ctx, 20, height - 108, "Memory: OK | Graphics: OK | Text: OK", 0xFFFFFFFF, 0x00000000);
}

static void roulette_handoff_to_demo(const struct roulette_backend *b, int width, int height) {
    /* Clear to the boot/demo background so the OS can continue with a clean slate. */
    b->fill_rect(b->ctx, 0, 0, width, height, ROULETTE_BG_COLOR);
    b->draw_text(b->ctx, width / 2 - 140, height / 2 - 20, "Shell launching... enjoy the demo", ROULETTE_TEXT_COLOR, 0x00000000);
    b->sleep_ms(b->ctx, 400);
    roulette_draw_demo_scene(b, width, height);
}

int roulette_run(const struct roulette_backend *backend, uint32_t fate_number) {
    if (!backend || !backend->get_size) {
        return -1;
    }

    int width = 0, height = 0;
    if (backend->get_size(backend->ctx, &width, &height) != 0 || width <= 0 || height <= 0) {
        return -1;
    }

    if (backend->fill_rect(backend->ctx, 0, 0, width, height, ROULETTE_BG_COLOR) != 0) {
        return -1;
    }

    backend->draw_text(backend->ctx, width / 2 - 150, 50, "=== THE WHEEL OF FATE ===", ROULETTE_WHEEL_COLOR, 0x00000000);
    backend->draw_text(backend->ctx, width / 2 - 120, 80, "Pointers choose your destiny...", ROULETTE_TEXT_COLOR, 0x00000000);

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

    backend->sleep_ms(backend->ctx, 300);

    int center_x = width / 2;
    int center_y = height / 2;
    int start_angle = segment_center_angle(start_segment);
    int target_angle = segment_center_angle(target_segment);
    int rotation_to_target = roulette_normalize_angle(target_angle - start_angle);
    int total_rotation = ROULETTE_SPIN_LOOPS * ROULETTE_DEGREE_STEPS + rotation_to_target;
    if (total_rotation <= 0) {
        total_rotation += ROULETTE_DEGREE_STEPS;
    }

    int last_pointer_angle = -1;
    render_wheel_frame(backend, width, height, center_x, center_y, radius,
                       -1, start_angle, &last_pointer_angle, fate_number, false, true, true);

    int total_frames = ROULETTE_SPIN_DURATION_MS / ROULETTE_SPIN_FRAME_DELAY_MS;
    if (total_frames < 1) {
        total_frames = 1;
    }

    for (int frame = 1; frame <= total_frames; frame++) {
        uint32_t p_q16 = ((uint32_t)frame << 16) / (uint32_t)total_frames;              /* progress in Q16 */
        uint32_t eased_q16 = (p_q16 * (131072u - p_q16)) >> 16;                         /* p * (2 - p) */
        int pointer_angle_frame = start_angle + (int)(((uint64_t)total_rotation * eased_q16) >> 16);
        render_wheel_frame(backend, width, height, center_x, center_y, radius,
                           -1, pointer_angle_frame, &last_pointer_angle,
                           fate_number, false, false, false);
        backend->sleep_ms(backend->ctx, ROULETTE_SPIN_FRAME_DELAY_MS);
    }

    int pointer_angle = start_angle + total_rotation;
    int landing_segment = target_segment;
    render_wheel_frame(backend, width, height, center_x, center_y, radius,
                       landing_segment, pointer_angle, &last_pointer_angle,
                       fate_number, false, true, true);
    backend->sleep_ms(backend->ctx, 500);

    backend->sleep_ms(backend->ctx, 400);

    for (int flash = 0; flash < 5; flash++) {
        render_wheel_frame(backend, width, height, center_x, center_y, radius,
                           landing_segment, pointer_angle, &last_pointer_angle,
                           fate_number, true, false, false);
        backend->sleep_ms(backend->ctx, 250);
        if (flash < 4) {
            render_wheel_frame(backend, width, height, center_x, center_y, radius,
                               landing_segment, pointer_angle, &last_pointer_angle,
                               fate_number, false, false, false);
            backend->sleep_ms(backend->ctx, 150);
        }
    }
    render_wheel_frame(backend, width, height, center_x, center_y, radius,
                       landing_segment, pointer_angle, &last_pointer_angle,
                       fate_number, true, false, true);
    backend->sleep_ms(backend->ctx, 600);

    int info_y = center_y + radius + 60;
    if (info_y < 0) {
        info_y = 0;
    }
    if (info_y > height) {
        info_y = height;
    }
    backend->fill_rect(backend->ctx, 0, info_y, width, height - info_y, ROULETTE_BG_COLOR);
    draw_result_banner(backend, center_x, center_y + radius + 80, fate_number);

    const char *currency_text = (fate_number & 1) ? "+10 W's (currency units)" : "-10 W's (currency units)";
    backend->draw_text(backend->ctx, center_x - 110, center_y + radius + 170, currency_text, ROULETTE_TEXT_COLOR, 0x00000000);

    if ((fate_number & 1) == 0) {
        backend->draw_text(backend->ctx, center_x - 130, center_y + radius + 210, "Press RESET to try again...", 0xFFFF00FF, 0x00000000);
    } else {
        backend->draw_text(backend->ctx, center_x - 130, center_y + radius + 210, "Continuing to OS...", 0x00FF00FF, 0x00000000);
    }

    backend->sleep_ms(backend->ctx, ROULETTE_RESULT_DELAY_MS);

    /* On wins, hand off to the familiar demo screen to show progress into OS. */
    if ((fate_number & 1) != 0) {
        roulette_handoff_to_demo(backend, width, height);
    }

    return 0;
}

