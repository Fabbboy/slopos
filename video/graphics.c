/*
 * SlopOS Graphics Primitives - Basic Drawing Operations
 * Provides fundamental drawing operations for framebuffer rendering
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../drivers/serial.h"
#include "framebuffer.h"
#include "graphics.h"

/* ========================================================================
 * UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Absolute value for integers
 */
static inline int abs(int x) {
    return x < 0 ? -x : x;
}

/*
 * Swap two integers
 */
static inline void swap_int(int *a, int *b) {
    int temp = *a;
    *a = *b;
    *b = temp;
}

/*
 * Check if coordinates are within framebuffer bounds
 */
static int bounds_check(int x, int y) {
    if (!framebuffer_is_initialized()) {
        return 0;
    }

    uint32_t width = framebuffer_get_width();
    uint32_t height = framebuffer_get_height();

    return (x >= 0 && x < (int)width && y >= 0 && y < (int)height);
}

/*
 * Clip coordinates to framebuffer bounds
 */
static void clip_coords(int *x, int *y) {
    uint32_t width = framebuffer_get_width();
    uint32_t height = framebuffer_get_height();

    if (*x < 0) *x = 0;
    if (*y < 0) *y = 0;
    if (*x >= (int)width) *x = width - 1;
    if (*y >= (int)height) *y = height - 1;
}

/* ========================================================================
 * BASIC DRAWING PRIMITIVES
 * ======================================================================== */

/*
 * Draw a single pixel (with bounds checking)
 */
int graphics_draw_pixel(int x, int y, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    if (!bounds_check(x, y)) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    framebuffer_set_pixel((uint32_t)x, (uint32_t)y, color);
    return GRAPHICS_SUCCESS;
}

/*
 * Draw a horizontal line
 */
int graphics_draw_hline(int x1, int x2, int y, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    if (!bounds_check(x1, y) && !bounds_check(x2, y)) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    /* Ensure x1 <= x2 */
    if (x1 > x2) {
        swap_int(&x1, &x2);
    }

    /* Clip to framebuffer bounds */
    clip_coords(&x1, &y);
    clip_coords(&x2, &y);

    /* Draw the line */
    for (int x = x1; x <= x2; x++) {
        framebuffer_set_pixel((uint32_t)x, (uint32_t)y, color);
    }

    return GRAPHICS_SUCCESS;
}

/*
 * Draw a vertical line
 */
int graphics_draw_vline(int x, int y1, int y2, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    if (!bounds_check(x, y1) && !bounds_check(x, y2)) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    /* Ensure y1 <= y2 */
    if (y1 > y2) {
        swap_int(&y1, &y2);
    }

    /* Clip to framebuffer bounds */
    clip_coords(&x, &y1);
    clip_coords(&x, &y2);

    /* Draw the line */
    for (int y = y1; y <= y2; y++) {
        framebuffer_set_pixel((uint32_t)x, (uint32_t)y, color);
    }

    return GRAPHICS_SUCCESS;
}

/*
 * Draw a line using Bresenham's algorithm
 */
int graphics_draw_line(int x0, int y0, int x1, int y1, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    /* Check if any part of the line could be visible */
    uint32_t width = framebuffer_get_width();
    uint32_t height = framebuffer_get_height();

    /* Simple bounds check - if both points are outside same boundary, skip */
    if ((x0 < 0 && x1 < 0) || (y0 < 0 && y1 < 0) ||
        (x0 >= (int)width && x1 >= (int)width) ||
        (y0 >= (int)height && y1 >= (int)height)) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    /* Bresenham's line algorithm */
    int dx = abs(x1 - x0);
    int dy = abs(y1 - y0);
    int sx = x0 < x1 ? 1 : -1;
    int sy = y0 < y1 ? 1 : -1;
    int err = dx - dy;

    int x = x0;
    int y = y0;

    while (1) {
        /* Draw pixel if within bounds */
        if (bounds_check(x, y)) {
            framebuffer_set_pixel((uint32_t)x, (uint32_t)y, color);
        }

        /* Check if we've reached the end */
        if (x == x1 && y == y1) {
            break;
        }

        /* Calculate error and adjust coordinates */
        int e2 = 2 * err;

        if (e2 > -dy) {
            err -= dy;
            x += sx;
        }

        if (e2 < dx) {
            err += dx;
            y += sy;
        }
    }

    return GRAPHICS_SUCCESS;
}

/* ========================================================================
 * RECTANGLE DRAWING
 * ======================================================================== */

/*
 * Draw a rectangle outline
 */
int graphics_draw_rect(int x, int y, int width, int height, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    if (width <= 0 || height <= 0) {
        return GRAPHICS_ERROR_INVALID;
    }

    /* Draw the four sides */
    graphics_draw_hline(x, x + width - 1, y, color);                    /* Top */
    graphics_draw_hline(x, x + width - 1, y + height - 1, color);       /* Bottom */
    graphics_draw_vline(x, y, y + height - 1, color);                   /* Left */
    graphics_draw_vline(x + width - 1, y, y + height - 1, color);       /* Right */

    return GRAPHICS_SUCCESS;
}

/*
 * Draw a filled rectangle
 */
int graphics_draw_rect_filled(int x, int y, int width, int height, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    if (width <= 0 || height <= 0) {
        return GRAPHICS_ERROR_INVALID;
    }

    /* Calculate bounds */
    int x1 = x;
    int y1 = y;
    int x2 = x + width - 1;
    int y2 = y + height - 1;

    /* Clip to framebuffer bounds */
    uint32_t fb_width = framebuffer_get_width();
    uint32_t fb_height = framebuffer_get_height();

    if (x1 < 0) x1 = 0;
    if (y1 < 0) y1 = 0;
    if (x2 >= (int)fb_width) x2 = fb_width - 1;
    if (y2 >= (int)fb_height) y2 = fb_height - 1;

    /* Check if rectangle is visible */
    if (x1 > x2 || y1 > y2) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    /* Fill the rectangle */
    for (int row = y1; row <= y2; row++) {
        for (int col = x1; col <= x2; col++) {
            framebuffer_set_pixel((uint32_t)col, (uint32_t)row, color);
        }
    }

    return GRAPHICS_SUCCESS;
}

/*
 * Draw a filled rectangle - FAST VERSION
 * Uses direct memory access and single bounds check for performance
 */
int graphics_draw_rect_filled_fast(int x, int y, int width, int height, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    if (width <= 0 || height <= 0) {
        return GRAPHICS_ERROR_INVALID;
    }

    /* Get framebuffer info directly */
    framebuffer_info_t *fb = framebuffer_get_info();
    if (!fb) return GRAPHICS_ERROR_NO_FB;

    /* Calculate bounds */
    int x1 = x;
    int y1 = y;
    int x2 = x + width - 1;
    int y2 = y + height - 1;

    /* Clip to framebuffer bounds */
    if (x1 < 0) x1 = 0;
    if (y1 < 0) y1 = 0;
    if (x2 >= (int)fb->width) x2 = fb->width - 1;
    if (y2 >= (int)fb->height) y2 = fb->height - 1;

    /* Check if rectangle is visible */
    if (x1 > x2 || y1 > y2) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    /* Pre-calculate color value */
    uint32_t pixel_value = color;
    if (fb->pixel_format == PIXEL_FORMAT_BGR ||
        fb->pixel_format == PIXEL_FORMAT_BGRA) {
        pixel_value = ((color & 0xFF0000) >> 16) |
                     (color & 0x00FF00) |
                     ((color & 0x0000FF) << 16) |
                     (color & 0xFF000000);
    }

    uint8_t *buffer = (uint8_t*)fb->virtual_addr;
    uint32_t bytes_pp = (fb->bpp + 7) / 8;
    uint32_t pitch = fb->pitch;

    /* 
     * Optimization: If we're filling 32-bit color, we can use a tighter loop
     * or even memset if the color is uniform (e.g. 0 or -1), but for now
     * we'll just do a tight loop per row to avoid function call overhead.
     */
    
    for (int row = y1; row <= y2; row++) {
        uint8_t *pixel_ptr = buffer + row * pitch + x1 * bytes_pp;
        
        /* Unroll loop for 32-bit color (most common) */
        if (bytes_pp == 4) {
            uint32_t *ptr32 = (uint32_t*)pixel_ptr;
            int count = x2 - x1 + 1;
            while (count--) {
                *ptr32++ = pixel_value;
            }
        } else {
            /* Fallback for other depths */
            for (int col = x1; col <= x2; col++) {
                switch (bytes_pp) {
                    case 2: *(uint16_t*)pixel_ptr = (uint16_t)pixel_value; break;
                    case 3: 
                        pixel_ptr[0] = (pixel_value >> 16) & 0xFF;
                        pixel_ptr[1] = (pixel_value >> 8) & 0xFF;
                        pixel_ptr[2] = pixel_value & 0xFF;
                        break;
                }
                pixel_ptr += bytes_pp;
            }
        }
    }

    return GRAPHICS_SUCCESS;
}

/* ========================================================================
 * CIRCLE DRAWING
 * ======================================================================== */

/*
 * Draw a circle outline using midpoint circle algorithm
 */
int graphics_draw_circle(int cx, int cy, int radius, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    if (radius <= 0) {
        return GRAPHICS_ERROR_INVALID;
    }

    /* Midpoint circle algorithm */
    int x = 0;
    int y = radius;
    int d = 1 - radius;

    /* Draw initial points */
    if (bounds_check(cx, cy + radius)) framebuffer_set_pixel(cx, cy + radius, color);
    if (bounds_check(cx, cy - radius)) framebuffer_set_pixel(cx, cy - radius, color);
    if (bounds_check(cx + radius, cy)) framebuffer_set_pixel(cx + radius, cy, color);
    if (bounds_check(cx - radius, cy)) framebuffer_set_pixel(cx - radius, cy, color);

    while (x < y) {
        if (d < 0) {
            d += 2 * x + 3;
        } else {
            d += 2 * (x - y) + 5;
            y--;
        }
        x++;

        /* Draw 8 octants */
        if (bounds_check(cx + x, cy + y)) framebuffer_set_pixel(cx + x, cy + y, color);
        if (bounds_check(cx - x, cy + y)) framebuffer_set_pixel(cx - x, cy + y, color);
        if (bounds_check(cx + x, cy - y)) framebuffer_set_pixel(cx + x, cy - y, color);
        if (bounds_check(cx - x, cy - y)) framebuffer_set_pixel(cx - x, cy - y, color);
        if (bounds_check(cx + y, cy + x)) framebuffer_set_pixel(cx + y, cy + x, color);
        if (bounds_check(cx - y, cy + x)) framebuffer_set_pixel(cx - y, cy + x, color);
        if (bounds_check(cx + y, cy - x)) framebuffer_set_pixel(cx + y, cy - x, color);
        if (bounds_check(cx - y, cy - x)) framebuffer_set_pixel(cx - y, cy - x, color);
    }

    return GRAPHICS_SUCCESS;
}

/*
 * Draw a filled circle
 */
int graphics_draw_circle_filled(int cx, int cy, int radius, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    if (radius <= 0) {
        return GRAPHICS_ERROR_INVALID;
    }

    /* Simple filled circle using distance check */
    int radius_sq = radius * radius;

    for (int y = -radius; y <= radius; y++) {
        for (int x = -radius; x <= radius; x++) {
            if (x * x + y * y <= radius_sq) {
                int px = cx + x;
                int py = cy + y;
                if (bounds_check(px, py)) {
                    framebuffer_set_pixel((uint32_t)px, (uint32_t)py, color);
                }
            }
        }
    }

    return GRAPHICS_SUCCESS;
}

/* ========================================================================
 * ADVANCED DRAWING FUNCTIONS
 * ======================================================================== */

/*
 * Draw a triangle outline
 */
int graphics_draw_triangle(int x1, int y1, int x2, int y2, int x3, int y3, uint32_t color) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    /* Draw the three sides */
    graphics_draw_line(x1, y1, x2, y2, color);
    graphics_draw_line(x2, y2, x3, y3, color);
    graphics_draw_line(x3, y3, x1, y1, color);

    return GRAPHICS_SUCCESS;
}

/*
 * Clear a rectangular region
 */
int graphics_clear_region(int x, int y, int width, int height, uint32_t color) {
    return graphics_draw_rect_filled(x, y, width, height, color);
}

/*
 * Draw a pattern-filled rectangle
 */
int graphics_draw_rect_pattern(int x, int y, int width, int height,
                              uint32_t color1, uint32_t color2, uint8_t pattern) {
    if (!framebuffer_is_initialized()) {
        return GRAPHICS_ERROR_NO_FB;
    }

    if (width <= 0 || height <= 0) {
        return GRAPHICS_ERROR_INVALID;
    }

    /* Calculate bounds and clip */
    int x1 = x;
    int y1 = y;
    int x2 = x + width - 1;
    int y2 = y + height - 1;

    uint32_t fb_width = framebuffer_get_width();
    uint32_t fb_height = framebuffer_get_height();

    if (x1 < 0) x1 = 0;
    if (y1 < 0) y1 = 0;
    if (x2 >= (int)fb_width) x2 = fb_width - 1;
    if (y2 >= (int)fb_height) y2 = fb_height - 1;

    if (x1 > x2 || y1 > y2) {
        return GRAPHICS_ERROR_BOUNDS;
    }

    /* Draw pattern */
    for (int row = y1; row <= y2; row++) {
        for (int col = x1; col <= x2; col++) {
            uint32_t pixel_color = color1;

            switch (pattern) {
                case FILL_HORIZONTAL_LINES:
                    pixel_color = (row % 2) ? color1 : color2;
                    break;
                case FILL_VERTICAL_LINES:
                    pixel_color = (col % 2) ? color1 : color2;
                    break;
                case FILL_DIAGONAL_LINES:
                    pixel_color = ((row + col) % 2) ? color1 : color2;
                    break;
                case FILL_CHECKERBOARD:
                    pixel_color = (((row / 8) + (col / 8)) % 2) ? color1 : color2;
                    break;
                default:
                    pixel_color = color1;
                    break;
            }

            framebuffer_set_pixel((uint32_t)col, (uint32_t)row, pixel_color);
        }
    }

    return GRAPHICS_SUCCESS;
}