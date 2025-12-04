/*
 * SlopOS Font Rendering Header
 * Interface for text rendering system
 */

#ifndef VIDEO_FONT_H
#define VIDEO_FONT_H

#include <stdint.h>

/* ========================================================================
 * FONT CONSTANTS
 * ======================================================================== */

/* Font dimensions */
#define FONT_CHAR_WIDTH     8     /* Character width in pixels */
#define FONT_CHAR_HEIGHT    16    /* Character height in pixels */
#define FONT_FIRST_CHAR     32    /* First printable character (space) */
#define FONT_LAST_CHAR      126   /* Last printable character (~) */

/* Font error codes */
#define FONT_SUCCESS        0
#define FONT_ERROR_NO_FB    -1
#define FONT_ERROR_BOUNDS   -2
#define FONT_ERROR_INVALID  -3

/* ========================================================================
 * TEXT RENDERING FUNCTIONS
 * ======================================================================== */

/*
 * Draw a single character at specified position
 * fg_color: foreground color
 * bg_color: background color (use 0 for transparent background)
 */
int font_draw_char(int x, int y, char c, uint32_t fg_color, uint32_t bg_color);

/*
 * Draw a string at specified position
 * Handles newlines, carriage returns, and tabs
 */
int font_draw_string(int x, int y, const char *str, uint32_t fg_color, uint32_t bg_color);

/*
 * Draw a string with background clearing
 * Clears the background area before drawing text
 */
int font_draw_string_clear(int x, int y, const char *str, uint32_t fg_color, uint32_t bg_color);

/*
 * Get string width in pixels
 * Returns width excluding newlines
 */
int font_get_string_width(const char *str);

/*
 * Get number of lines in string
 */
int font_get_string_lines(const char *str);

/* ========================================================================
 * CONSOLE-STYLE TEXT OUTPUT
 * ======================================================================== */

/*
 * Initialize console with colors
 */
void font_console_init(uint32_t fg_color, uint32_t bg_color);

/*
 * Print character to console
 * Handles cursor movement and line wrapping
 */
int font_console_putc(char c);

/*
 * Print string to console
 */
int font_console_puts(const char *str);

/*
 * Clear console and reset cursor
 */
int font_console_clear(void);

/*
 * Set console colors
 */
void font_console_set_colors(uint32_t fg_color, uint32_t bg_color);

#endif /* VIDEO_FONT_H */