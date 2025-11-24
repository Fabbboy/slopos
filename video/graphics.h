/*
 * SlopOS Graphics Primitives Header
 * Interface for basic drawing operations
 */

#ifndef VIDEO_GRAPHICS_H
#define VIDEO_GRAPHICS_H

#include <stdint.h>

/* ========================================================================
 * GRAPHICS CONSTANTS
 * ======================================================================== */

/* Fill patterns */
#define FILL_SOLID                  0x00
#define FILL_HORIZONTAL_LINES       0x01
#define FILL_VERTICAL_LINES         0x02
#define FILL_DIAGONAL_LINES         0x03
#define FILL_CHECKERBOARD           0x04

/* Graphics error codes */
#define GRAPHICS_SUCCESS            0
#define GRAPHICS_ERROR_NO_FB        -1
#define GRAPHICS_ERROR_BOUNDS       -2
#define GRAPHICS_ERROR_INVALID      -3

/* ========================================================================
 * BASIC DRAWING PRIMITIVES
 * ======================================================================== */

/*
 * Draw a single pixel (with bounds checking)
 */
int graphics_draw_pixel(int x, int y, uint32_t color);

/*
 * Draw a horizontal line
 */
int graphics_draw_hline(int x1, int x2, int y, uint32_t color);

/*
 * Draw a vertical line
 */
int graphics_draw_vline(int x, int y1, int y2, uint32_t color);

/*
 * Draw a line using Bresenham's algorithm
 */
int graphics_draw_line(int x0, int y0, int x1, int y1, uint32_t color);

/* ========================================================================
 * RECTANGLE DRAWING
 * ======================================================================== */

/*
 * Draw a rectangle outline
 */
int graphics_draw_rect(int x, int y, int width, int height, uint32_t color);

/*
 * Draw a filled rectangle
 */
int graphics_draw_rect_filled(int x, int y, int width, int height, uint32_t color);
int graphics_draw_rect_filled_fast(int x, int y, int width, int height, uint32_t color);

/*
 * Draw a pattern-filled rectangle
 */
int graphics_draw_rect_pattern(int x, int y, int width, int height,
                              uint32_t color1, uint32_t color2, uint8_t pattern);

/* ========================================================================
 * CIRCLE DRAWING
 * ======================================================================== */

/*
 * Draw a circle outline using midpoint circle algorithm
 */
int graphics_draw_circle(int cx, int cy, int radius, uint32_t color);

/*
 * Draw a filled circle
 */
int graphics_draw_circle_filled(int cx, int cy, int radius, uint32_t color);

/* ========================================================================
 * ADVANCED DRAWING FUNCTIONS
 * ======================================================================== */

/*
 * Draw a triangle outline
 */
int graphics_draw_triangle(int x1, int y1, int x2, int y2, int x3, int y3, uint32_t color);

/*
 * Clear a rectangular region
 */
int graphics_clear_region(int x, int y, int width, int height, uint32_t color);

#endif /* VIDEO_GRAPHICS_H */