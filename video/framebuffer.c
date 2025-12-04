/*
 * SlopOS Framebuffer Driver - UEFI GOP Framebuffer Management
 * Handles initialization and management of the system framebuffer
 *
 * DEPENDENCY: This driver requires HHDM (Higher-Half Direct Mapping) or
 * identity mapping to access the framebuffer. The framebuffer physical address
 * is translated to a virtual address using mm_phys_to_virt(). If no mapping
 * is available, framebuffer initialization will fail gracefully.
 *
 * NOTE: The framebuffer memory range should ideally be reserved during memory
 * system initialization to prevent it from being allocated by the page allocator.
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../boot/limine_protocol.h"
#include "../boot/log.h"
#include "../drivers/serial.h"
#include "../mm/phys_virt.h"
#include "framebuffer.h"

/* Forward declarations */
void kernel_panic(const char *message);
int get_framebuffer_info(uint64_t *addr, uint32_t *width, uint32_t *height,
                        uint32_t *pitch, uint8_t *bpp);

/* ========================================================================
 * FRAMEBUFFER CONSTANTS AND STRUCTURES
 * ======================================================================== */

/* Global framebuffer state */

/* Global framebuffer state */
static framebuffer_info_t fb_info = {0};

/* ========================================================================
 * PIXEL FORMAT UTILITIES
 * ======================================================================== */

/*
 * Determine pixel format from bits per pixel
 */
static uint8_t determine_pixel_format(uint8_t bpp) {
    switch (bpp) {
        case 16:
            return PIXEL_FORMAT_RGB;    /* Assume RGB565 */
        case 24:
            return PIXEL_FORMAT_RGB;    /* RGB888 */
        case 32:
            return PIXEL_FORMAT_RGBA;   /* RGBA8888 or RGBX8888 */
        default:
            return PIXEL_FORMAT_RGB;    /* Default fallback */
    }
}

/*
 * Calculate bytes per pixel from bits per pixel
 */
static uint32_t bytes_per_pixel(uint8_t bpp) {
    return (bpp + 7) / 8;  /* Round up to nearest byte */
}

/*
 * Convert color from RGB to hardware pixel format
 * Swaps red and blue channels for BGR/BGRA framebuffers
 */
uint32_t framebuffer_convert_color(uint32_t color) {
    if (!fb_info.initialized) {
        return color;
    }
    if (fb_info.pixel_format == PIXEL_FORMAT_BGR ||
        fb_info.pixel_format == PIXEL_FORMAT_BGRA) {
        /* Swap R and B components */
        return ((color & 0xFF0000) >> 16) |
               (color & 0x00FF00) |
               ((color & 0x0000FF) << 16) |
               (color & 0xFF000000);
    }
    return color;
}

/*
 * Validate framebuffer dimensions
 */
static int validate_dimensions(uint32_t width, uint32_t height) {
    if (width < MIN_FRAMEBUFFER_WIDTH || width > MAX_FRAMEBUFFER_WIDTH) {
        return 0;
    }
    if (height < MIN_FRAMEBUFFER_HEIGHT || height > MAX_FRAMEBUFFER_HEIGHT) {
        return 0;
    }
    return 1;
}

/* ========================================================================
 * FRAMEBUFFER INITIALIZATION
 * ======================================================================== */

/*
 * Initialize framebuffer from Multiboot2 information
 */
int framebuffer_init(void) {
    uint64_t phys_addr;
    uint32_t width, height, pitch;
    uint8_t bpp;

    boot_log_debug("Initializing framebuffer...");

    /* Get framebuffer info from Multiboot2 */
    if (!get_framebuffer_info(&phys_addr, &width, &height, &pitch, &bpp)) {
        boot_log_info("ERROR: No framebuffer available from bootloader");
        return -1;
    }

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Framebuffer address from bootloader: ");
        kprint_hex(phys_addr);
        kprintln("");
    });

    /* Validate parameters */
    if (phys_addr == 0) {
        boot_log_info("ERROR: Invalid framebuffer address");
        return -1;
    }

    if (!validate_dimensions(width, height)) {
        boot_log_info("ERROR: Invalid framebuffer dimensions");
        return -1;
    }

    if (bpp != 16 && bpp != 24 && bpp != 32) {
        boot_log_info("ERROR: Unsupported color depth");
        return -1;
    }

    /* Calculate buffer size */
    uint32_t buffer_size = pitch * height;
    if (buffer_size == 0 || buffer_size > 64 * 1024 * 1024) {  /* Max 64MB */
        boot_log_info("ERROR: Invalid framebuffer size");
        return -1;
    }

    /*
     * Translate physical framebuffer address to virtual address.
     * Limine provides the framebuffer address as a virtual address (via HHDM)
     * when HHDM is available, so we need to detect this and handle it correctly.
     */
    uint64_t virtual_addr_uint;
    uint64_t physical_addr_for_storage;
    
    /* Check if the address from Limine is already a virtual address (via HHDM) */
    if (is_hhdm_available()) {
        uint64_t hhdm_offset = get_hhdm_offset();
        /* 
         * Limine provides framebuffer addresses as virtual addresses (via HHDM)
         * when HHDM is available. If the address is >= HHDM offset, it's already
         * virtual. Otherwise, treat it as physical and convert it.
         */
        if (phys_addr >= hhdm_offset) {
            /* Address is already virtual via HHDM, use it directly */
            virtual_addr_uint = phys_addr;
            /* Calculate physical address for storage */
            physical_addr_for_storage = phys_addr - hhdm_offset;
        } else {
            /* Address appears to be physical, convert it */
            virtual_addr_uint = mm_phys_to_virt(phys_addr);
            physical_addr_for_storage = phys_addr;
        }
    } else {
        /* No HHDM, treat as physical and convert */
        virtual_addr_uint = mm_phys_to_virt(phys_addr);
        physical_addr_for_storage = phys_addr;
    }
    
    if (virtual_addr_uint == 0) {
        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_INFO, {
            kprint("ERROR: No virtual mapping available for framebuffer at address ");
            kprint_hex(phys_addr);
            kprintln("");
            kprintln("Framebuffer requires HHDM (Higher-Half Direct Mapping) or identity mapping");
        });
        return -1;
    }
    void *virtual_addr = (void*)virtual_addr_uint;

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Framebuffer virtual address: ");
        kprint_hex(virtual_addr_uint);
        kprintln("");
    });

    /* Initialize framebuffer info */
    fb_info.physical_addr = physical_addr_for_storage;
    fb_info.virtual_addr = virtual_addr;
    fb_info.width = width;
    fb_info.height = height;
    fb_info.pitch = pitch;
    fb_info.bpp = bpp;
    fb_info.pixel_format = determine_pixel_format(bpp);
    fb_info.buffer_size = buffer_size;
    fb_info.initialized = 1;

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Framebuffer initialized: ");
        kprint_decimal(width);
        kprint("x");
        kprint_decimal(height);
        kprint(" @ ");
        kprint_decimal(bpp);
        kprintln(" bpp");
    });

    return 0;
}

/*
 * Get framebuffer information
 */
framebuffer_info_t *framebuffer_get_info(void) {
    if (!fb_info.initialized) {
        return NULL;
    }
    return &fb_info;
}

/*
 * Check if framebuffer is initialized
 */
int framebuffer_is_initialized(void) {
    return fb_info.initialized;
}

/* ========================================================================
 * BASIC FRAMEBUFFER OPERATIONS
 * ======================================================================== */

/*
 * Clear framebuffer to specified color
 */
void framebuffer_clear(uint32_t color) {
    if (!fb_info.initialized) {
        return;
    }

    uint8_t *buffer = (uint8_t*)fb_info.virtual_addr;
    uint32_t bytes_pp = bytes_per_pixel(fb_info.bpp);
    uint32_t pixel_value = framebuffer_convert_color(color);

    /* Fill buffer */
    for (uint32_t y = 0; y < fb_info.height; y++) {
        uint8_t *row = buffer + y * fb_info.pitch;

        for (uint32_t x = 0; x < fb_info.width; x++) {
            uint8_t *pixel = row + x * bytes_pp;

            switch (bytes_pp) {
                case 2: /* 16-bit */
                    *(uint16_t*)pixel = (uint16_t)pixel_value;
                    break;
                case 3: /* 24-bit */
                    pixel[0] = (pixel_value >> 16) & 0xFF;
                    pixel[1] = (pixel_value >> 8) & 0xFF;
                    pixel[2] = pixel_value & 0xFF;
                    break;
                case 4: /* 32-bit */
                    *(uint32_t*)pixel = pixel_value;
                    break;
            }
        }
    }
}

/*
 * Set pixel at coordinates
 */
void framebuffer_set_pixel(uint32_t x, uint32_t y, uint32_t color) {
    if (!fb_info.initialized) {
        return;
    }

    /* Bounds checking */
    if (x >= fb_info.width || y >= fb_info.height) {
        return;
    }

    uint8_t *buffer = (uint8_t*)fb_info.virtual_addr;
    uint32_t bytes_pp = bytes_per_pixel(fb_info.bpp);
    uint8_t *pixel = buffer + y * fb_info.pitch + x * bytes_pp;
    uint32_t pixel_value = framebuffer_convert_color(color);

    /* Set pixel */
    switch (bytes_pp) {
        case 2: /* 16-bit */
            *(uint16_t*)pixel = (uint16_t)pixel_value;
            break;
        case 3: /* 24-bit */
            pixel[0] = (pixel_value >> 16) & 0xFF;
            pixel[1] = (pixel_value >> 8) & 0xFF;
            pixel[2] = pixel_value & 0xFF;
            break;
        case 4: /* 32-bit */
            *(uint32_t*)pixel = pixel_value;
            break;
    }
}

/*
 * Get pixel at coordinates
 */
uint32_t framebuffer_get_pixel(uint32_t x, uint32_t y) {
    if (!fb_info.initialized) {
        return 0;
    }

    /* Bounds checking */
    if (x >= fb_info.width || y >= fb_info.height) {
        return 0;
    }

    uint8_t *buffer = (uint8_t*)fb_info.virtual_addr;
    uint32_t bytes_pp = bytes_per_pixel(fb_info.bpp);
    uint8_t *pixel = buffer + y * fb_info.pitch + x * bytes_pp;

    uint32_t color = 0;

    /* Get pixel */
    switch (bytes_pp) {
        case 2: /* 16-bit */
            color = *(uint16_t*)pixel;
            break;
        case 3: /* 24-bit */
            color = (pixel[0] << 16) | (pixel[1] << 8) | pixel[2];
            break;
        case 4: /* 32-bit */
            color = *(uint32_t*)pixel;
            break;
    }

    /* Convert color from hardware format to RGB */
    return framebuffer_convert_color(color);
}

/* ========================================================================
 * FRAMEBUFFER UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Get framebuffer width
 */
uint32_t framebuffer_get_width(void) {
    return fb_info.initialized ? fb_info.width : 0;
}

/*
 * Get framebuffer height
 */
uint32_t framebuffer_get_height(void) {
    return fb_info.initialized ? fb_info.height : 0;
}

/*
 * Get color depth
 */
uint8_t framebuffer_get_bpp(void) {
    return fb_info.initialized ? fb_info.bpp : 0;
}

/*
 * Create RGBA color value
 */
uint32_t framebuffer_rgba(uint8_t r, uint8_t g, uint8_t b, uint8_t a) {
    return ((uint32_t)r << 16) | ((uint32_t)g << 8) | (uint32_t)b | ((uint32_t)a << 24);
}

/*
 * Create RGB color value (with full alpha)
 */
uint32_t framebuffer_rgb(uint8_t r, uint8_t g, uint8_t b) {
    return framebuffer_rgba(r, g, b, 0xFF);
}
