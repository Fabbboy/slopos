/*
 * SlopOS Framebuffer Driver Header
 * Interface for framebuffer initialization and basic operations
 */

#ifndef VIDEO_FRAMEBUFFER_H
#define VIDEO_FRAMEBUFFER_H

#include <stdint.h>
#include <stddef.h>

/* ========================================================================
 * FRAMEBUFFER CONSTANTS
 * ======================================================================== */

/* Supported pixel formats */
#define PIXEL_FORMAT_RGB    0x01   /* Red-Green-Blue */
#define PIXEL_FORMAT_BGR    0x02   /* Blue-Green-Red */
#define PIXEL_FORMAT_RGBA   0x03   /* Red-Green-Blue-Alpha */
#define PIXEL_FORMAT_RGBA   0x03   /* Red-Green-Blue-Alpha */
#define PIXEL_FORMAT_BGRA   0x04   /* Blue-Green-Red-Alpha */

/* Maximum framebuffer dimensions */
#define MAX_FRAMEBUFFER_WIDTH   4096
#define MAX_FRAMEBUFFER_HEIGHT  4096
#define MIN_FRAMEBUFFER_WIDTH   320
#define MIN_FRAMEBUFFER_HEIGHT  240

/* Common colors (32-bit RGBA format) */
#define COLOR_BLACK       0x00000000
#define COLOR_WHITE       0xFFFFFFFF
#define COLOR_RED         0xFF0000FF
#define COLOR_GREEN       0x00FF00FF
#define COLOR_BLUE        0x0000FFFF
#define COLOR_YELLOW      0xFFFF00FF
#define COLOR_CYAN        0x00FFFFFF
#define COLOR_MAGENTA     0xFF00FFFF
#define COLOR_GRAY        0x808080FF
#define COLOR_DARK_GRAY   0x404040FF
#define COLOR_LIGHT_GRAY  0xC0C0C0FF

/* ========================================================================
 * FRAMEBUFFER STRUCTURES
 * ======================================================================== */

/* Framebuffer information structure */
typedef struct {
    uint64_t physical_addr;     /* Physical address of framebuffer */
    void *virtual_addr;         /* Virtual address of framebuffer */
    uint32_t width;             /* Width in pixels */
    uint32_t height;            /* Height in pixels */
    uint32_t pitch;             /* Bytes per scanline */
    uint8_t bpp;                /* Bits per pixel */
    uint8_t pixel_format;       /* Pixel format */
    uint32_t buffer_size;       /* Total buffer size in bytes */
    uint8_t initialized;        /* Initialization status */
} framebuffer_info_t;

/* ========================================================================
 * FRAMEBUFFER INITIALIZATION
 * ======================================================================== */

/*
 * Initialize framebuffer from Multiboot2 information
 * Returns 0 on success, negative on error
 */
int framebuffer_init(void);

/*
 * Get framebuffer information
 * Returns pointer to info structure, NULL if not initialized
 */
framebuffer_info_t *framebuffer_get_info(void);

/*
 * Check if framebuffer is initialized
 * Returns non-zero if initialized, zero otherwise
 */
int framebuffer_is_initialized(void);

/* ========================================================================
 * BASIC FRAMEBUFFER OPERATIONS
 * ======================================================================== */

/*
 * Clear framebuffer to specified color
 */
void framebuffer_clear(uint32_t color);

/*
 * Set pixel at coordinates
 */
void framebuffer_set_pixel(uint32_t x, uint32_t y, uint32_t color);

/*
 * Get pixel at coordinates
 */
uint32_t framebuffer_get_pixel(uint32_t x, uint32_t y);

/* ========================================================================
 * FRAMEBUFFER UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Get framebuffer width
 */
uint32_t framebuffer_get_width(void);

/*
 * Get framebuffer height
 */
uint32_t framebuffer_get_height(void);

/*
 * Get color depth
 */
uint8_t framebuffer_get_bpp(void);

/*
 * Create RGBA color value
 */
uint32_t framebuffer_rgba(uint8_t r, uint8_t g, uint8_t b, uint8_t a);

/*
 * Create RGB color value (with full alpha)
 */
uint32_t framebuffer_rgb(uint8_t r, uint8_t g, uint8_t b);

#endif /* VIDEO_FRAMEBUFFER_H */