/*
 * Shared roulette rendering core.
 * A backend provides drawing and timing ops; the core renders the full wheel.
 */

#ifndef VIDEO_ROULETTE_CORE_H
#define VIDEO_ROULETTE_CORE_H

#include <stdint.h>

struct roulette_backend {
    void *ctx;
    /* Return 0 on success, non-zero on failure. */
    int (*get_size)(void *ctx, int *width, int *height);
    int (*fill_rect)(void *ctx, int x, int y, int w, int h, uint32_t color);
    int (*draw_line)(void *ctx, int x0, int y0, int x1, int y1, uint32_t color);
    int (*draw_circle)(void *ctx, int cx, int cy, int radius, uint32_t color);
    int (*draw_circle_filled)(void *ctx, int cx, int cy, int radius, uint32_t color);
    int (*draw_text)(void *ctx, int x, int y, const char *text, uint32_t fg, uint32_t bg);
    void (*sleep_ms)(void *ctx, uint32_t ms);
};

int roulette_run(const struct roulette_backend *backend, uint32_t fate_number);

#endif /* VIDEO_ROULETTE_CORE_H */


