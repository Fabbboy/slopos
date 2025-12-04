/*
 * SlopOS Framebuffer Draw Task
 * Continuously renders animated primitives while yielding cooperatively.
 */

#include "../drivers/serial.h"
#include "../video/framebuffer.h"
#include "../video/graphics.h"
#include "scheduler.h"

/* Simple color palette for animation */
static const uint32_t kPalette[] = {
    COLOR_RED,
    COLOR_GREEN,
    COLOR_BLUE,
    COLOR_CYAN,
    COLOR_MAGENTA,
    COLOR_YELLOW,
    COLOR_WHITE
};

static inline uint32_t palette_color(uint32_t index) {
    return kPalette[index % (sizeof(kPalette) / sizeof(kPalette[0]))];
}

/*
 * Drawing task entry point.
 * Clears the framebuffer once, then animates a bouncing square and moving bar.
 */
static void framebuffer_draw_task(void *arg) {
    (void)arg;

    if (!framebuffer_is_initialized()) {
        kprintln("draw_task: framebuffer not initialized, parking task");
        while (1) {
            yield();
        }
    }

    const uint32_t width = framebuffer_get_width();
    const uint32_t height = framebuffer_get_height();

    /* Draw static border and clear background once */
    framebuffer_clear(COLOR_BLACK);
    graphics_draw_rect_filled(0, 0, (int)width, 3, COLOR_WHITE);
    graphics_draw_rect_filled(0, (int)height - 3, (int)width, 3, COLOR_WHITE);
    graphics_draw_rect_filled(0, 0, 3, (int)height, COLOR_WHITE);
    graphics_draw_rect_filled((int)width - 3, 0, 3, (int)height, COLOR_WHITE);

    const int square_size = 96;
    int square_x = 40;
    int square_y = 40;
    int square_dx = 6;
    int square_dy = 5;
    uint32_t palette_index = 0;

    uint32_t scan_offset = 0;

    while (1) {
        /* Clear previous square footprint */
        graphics_draw_rect_filled(square_x, square_y, square_size, square_size, COLOR_BLACK);

        /* Update square position and bounce within bounds */
        square_x += square_dx;
        square_y += square_dy;

        if (square_x <= 3) {
            square_x = 3;
            square_dx = -square_dx;
        } else if (square_x + square_size >= (int)width - 3) {
            square_x = (int)width - 3 - square_size;
            square_dx = -square_dx;
        }

        if (square_y <= 3) {
            square_y = 3;
            square_dy = -square_dy;
        } else if (square_y + square_size >= (int)height - 3) {
            square_y = (int)height - 3 - square_size;
            square_dy = -square_dy;
        }

        /* Draw the animated square */
        const uint32_t square_color = palette_color(palette_index++);
        graphics_draw_rect_filled(square_x, square_y, square_size, square_size, square_color);

        /* Animate a horizontal scanning bar near the bottom */
        const int bar_height = 32;
        const int bar_y = (int)height - 3 - bar_height - 16;
        graphics_draw_rect_filled(3, bar_y, (int)width - 6, bar_height, COLOR_BLACK);

        const int bar_width = width / 5;
        int bar_x = (int)(scan_offset % (width - 6));
        if (bar_x + bar_width > (int)width - 3) {
            bar_x = (int)width - 3 - bar_width;
        }
        graphics_draw_rect_filled(bar_x, bar_y, bar_width, bar_height, palette_color(palette_index));

        scan_offset = (scan_offset + 8) % (width ? width : 1);

        /* Simple delay to slow down the animation a bit */
        for (volatile uint32_t spin = 0; spin < 250000; spin++) {
            __asm__ volatile("pause");
        }

        yield();
    }
}

int spawn_framebuffer_draw_task(void) {
    if (!framebuffer_is_initialized()) {
        kprintln("spawn_draw_task: framebuffer not ready, skipping task creation");
        return -1;
    }

    uint32_t task_id = task_create(
        "fb_draw",
        framebuffer_draw_task,
        NULL,
        TASK_PRIORITY_LOW,
        TASK_FLAG_KERNEL_MODE
    );

    if (task_id == INVALID_TASK_ID) {
        kprintln("spawn_draw_task: failed to create task");
        return -1;
    }

    task_t *task_info = NULL;
    if (task_get_info(task_id, &task_info) != 0 || task_info == NULL) {
        kprintln("spawn_draw_task: failed to retrieve task info");
        return -1;
    }

    if (schedule_task(task_info) != 0) {
        kprintln("spawn_draw_task: schedule_task failed");
        return -1;
    }

    kprintln("spawn_draw_task: framebuffer draw task scheduled");
    return 0;
}

