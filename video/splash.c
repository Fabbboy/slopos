/*
 * SlopOS Splash Screen Implementation
 * Displays boot splash screen with logo and loading progress
 */

#include <stdint.h>
#include <stddef.h>
#include "splash.h"
#include "framebuffer.h"
#include "graphics.h"
#include "font.h"
#include "../drivers/serial.h"
#include "../drivers/pit.h"
#include "../lib/klog.h"

/* ========================================================================
 * SPLASH SCREEN IMPLEMENTATION
 * ======================================================================== */

/*
 * Draw SlopOS logo as ASCII art using graphics primitives
 */
static int splash_draw_logo(int center_x, int center_y) {
    if (!framebuffer_is_initialized()) {
        return -1;
    }

    // Calculate logo dimensions and position
    int logo_width = 300;
    int logo_height = 150;
    int logo_x = center_x - logo_width / 2;
    int logo_y = center_y - logo_height / 2;

    // Draw main logo rectangle with gradient effect
    for (int y = 0; y < logo_height; y++) {
        uint32_t gradient_intensity = 0x40 + (y * 0x80 / logo_height);
        uint32_t gradient_color = (gradient_intensity << 24) | (gradient_intensity << 16) | 0xFF;
        graphics_draw_hline(logo_x, logo_x + logo_width, logo_y + y, gradient_color);
    }

    // Draw logo border
    graphics_draw_rect(logo_x - 2, logo_y - 2, logo_width + 4, logo_height + 4, SPLASH_LOGO_COLOR);

    // Draw stylized "SLOP" letters using geometric shapes
    int letter_spacing = 60;
    int letter_start_x = logo_x + 30;
    int letter_y = logo_y + 40;
    int letter_height = 70;

    // Letter S - curves approximated with rectangles
    graphics_draw_rect_filled(letter_start_x, letter_y, 40, 15, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x, letter_y + 25, 40, 15, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x, letter_y + 55, 40, 15, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x, letter_y, 15, 40, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x + 25, letter_y + 30, 15, 40, SPLASH_LOGO_COLOR);

    // Letter L
    letter_start_x += letter_spacing;
    graphics_draw_rect_filled(letter_start_x, letter_y, 15, letter_height, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x, letter_y + letter_height - 15, 40, 15, SPLASH_LOGO_COLOR);

    // Letter O
    letter_start_x += letter_spacing;
    graphics_draw_rect_filled(letter_start_x, letter_y, 40, 15, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x, letter_y + letter_height - 15, 40, 15, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x, letter_y, 15, letter_height, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x + 25, letter_y, 15, letter_height, SPLASH_LOGO_COLOR);

    // Letter P
    letter_start_x += letter_spacing;
    graphics_draw_rect_filled(letter_start_x, letter_y, 15, letter_height, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x, letter_y, 40, 15, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x, letter_y + 25, 40, 15, SPLASH_LOGO_COLOR);
    graphics_draw_rect_filled(letter_start_x + 25, letter_y, 15, 40, SPLASH_LOGO_COLOR);

    return 0;
}

/*
 * Draw progress bar
 */
static int splash_draw_progress_bar(int x, int y, int width, int height, int progress) {
    if (!framebuffer_is_initialized()) {
        return -1;
    }

    // Draw progress bar background
    graphics_draw_rect_filled(x, y, width, height, 0x333333FF);

    // Draw progress bar border
    graphics_draw_rect(x - 1, y - 1, width + 2, height + 2, SPLASH_LOGO_COLOR);

    // Draw progress fill
    if (progress > 0) {
        int fill_width = (width * progress) / 100;
        graphics_draw_rect_filled(x, y, fill_width, height, SPLASH_PROGRESS_COLOR);
    }

    return 0;
}

// Global splash screen state
static int splash_active = 0;
static int current_progress = 0;

/*
 * Initialize splash screen (without fake animation)
 */
int splash_show_boot_screen(void) {
    if (!framebuffer_is_initialized()) {
        klog_printf(KLOG_INFO, "SPLASH: Framebuffer not initialized\n");
        return -1;
    }

    klog_printf(KLOG_INFO, "SPLASH: Displaying boot splash screen...\n");

    // Clear screen with splash background color
    framebuffer_clear(SPLASH_BG_COLOR);

    // Get screen dimensions
    uint32_t width = framebuffer_get_width();
    uint32_t height = framebuffer_get_height();
    int center_x = width / 2;
    int center_y = height / 2;

    // Draw logo
    splash_draw_logo(center_x, center_y - 80);

    // Draw title text
    font_draw_string(center_x - 80, center_y + 100, "SlopOS v0.000069", SPLASH_TEXT_COLOR, 0x00000000);
    font_draw_string(center_x - 120, center_y + 120, "the ultimate vibe slop experience", SPLASH_TEXT_COLOR, 0x00000000);

    // Draw loading message
    font_draw_string(center_x - 40, center_y + 160, "Initializing...", SPLASH_TEXT_COLOR, 0x00000000);

    // Draw initial progress bar at 0%
    int progress_bar_width = 300;
    int progress_bar_height = 20;
    int progress_bar_x = center_x - progress_bar_width / 2;
    int progress_bar_y = center_y + 200;

    splash_draw_progress_bar(progress_bar_x, progress_bar_y, progress_bar_width, progress_bar_height, 0);

    // Mark splash as active and reset progress
    splash_active = 1;
    current_progress = 0;

    klog_printf(KLOG_INFO, "SPLASH: Boot splash screen initialized\n");

    // No initial delay - let the boot process drive the timing

    return 0;
}

/*
 * Report progress during boot (called from kernel initialization)
 */
int splash_report_progress(int progress, const char *message) {
    if (!splash_active || !framebuffer_is_initialized()) {
        return -1;
    }

    // Update progress
    current_progress = progress;
    if (current_progress > 100) current_progress = 100;

    klog_printf(KLOG_INFO, "SPLASH: Progress %d%% - %s\n",
                current_progress,
                message ? message : "...");

    // Update the visual progress bar and message
    int result = splash_update_progress(current_progress, message);

    // Add brief delays between steps for 4 second total boot time
    // With 14 total steps, each delay should be ~285ms for 4 second total
    uint32_t delay_ms = 280; // Base delay - 0.28 seconds

    // Slight variation based on operation type for realistic feel
    if (current_progress <= 20) {
        delay_ms = 300; // Graphics initialization - 0.3 seconds
    } else if (current_progress <= 40) {
        delay_ms = 250; // Early system setup - 0.25 seconds
    } else if (current_progress <= 60) {
        delay_ms = 280; // APIC/interrupt setup - 0.28 seconds
    } else if (current_progress <= 80) {
        delay_ms = 320; // PCI enumeration takes longer - 0.32 seconds
    } else if (current_progress <= 95) {
        delay_ms = 280; // Scheduler/task setup - 0.28 seconds
    } else {
        delay_ms = 250; // Final completion - 0.25 seconds
    }

    // Apply the delay
    pit_poll_delay_ms(delay_ms);

    return result;
}

/*
 * Draw the post-boot graphics demo screen
 * Shows colored rectangles, circle, border, and welcome text
 */
int splash_draw_graphics_demo(void) {
    if (!framebuffer_is_initialized()) {
        return -1;
    }

    // Clear splash screen and show graphics demo (like in 8fe117b)
    framebuffer_clear(0x001122FF);

    // Initialize console with white text on dark background
    font_console_init(0xFFFFFFFF, 0x00000000);

    // Draw graphics demo
    graphics_draw_rect_filled(20, 20, 300, 150, 0xFF0000FF);        // Red rectangle
    graphics_draw_rect_filled(700, 20, 300, 150, 0x00FF00FF);       // Green rectangle
    graphics_draw_circle(512, 384, 100, 0xFFFF00FF);                // Yellow circle

    // White border around entire screen
    graphics_draw_rect_filled(0, 0, 1024, 4, 0xFFFFFFFF);           // Top
    graphics_draw_rect_filled(0, 764, 1024, 4, 0xFFFFFFFF);         // Bottom
    graphics_draw_rect_filled(0, 0, 4, 768, 0xFFFFFFFF);            // Left
    graphics_draw_rect_filled(1020, 0, 4, 768, 0xFFFFFFFF);         // Right

    // Display welcome message using font_draw_string
    font_draw_string(20, 600, "*** SLOPOS GRAPHICS SYSTEM OPERATIONAL ***", 0xFFFFFFFF, 0x00000000);
    font_draw_string(20, 616, "Framebuffer: WORKING | Resolution: 1024x768", 0xFFFFFFFF, 0x00000000);
    font_draw_string(20, 632, "Memory: OK | Graphics: OK | Text: OK", 0xFFFFFFFF, 0x00000000);

    return 0;
}

/*
 * Mark splash screen as complete
 */
int splash_finish(void) {
    if (splash_active) {
        splash_report_progress(100, "Boot complete");

        // Show "Boot complete" message for 0.25 seconds before finishing
        pit_poll_delay_ms(250);

        splash_active = 0;
        klog_printf(KLOG_INFO, "SPLASH: Boot splash screen complete\n");

        // Draw the graphics demo screen
        splash_draw_graphics_demo();
    }
    return 0;
}

/*
 * Update splash screen with loading progress
 */
int splash_update_progress(int progress, const char *message) {
    if (!framebuffer_is_initialized()) {
        return -1;
    }

    // Get screen dimensions
    uint32_t width = framebuffer_get_width();
    uint32_t height = framebuffer_get_height();
    int center_x = width / 2;
    int center_y = height / 2;

    // Clear previous message area
    graphics_draw_rect_filled(center_x - 150, center_y + 155, 300, 20, SPLASH_BG_COLOR);

    // Draw new message
    if (message) {
        font_draw_string(center_x - 70, center_y + 160, message, SPLASH_TEXT_COLOR, 0x00000000);
    }

    // Update progress bar
    int progress_bar_width = 300;
    int progress_bar_height = 20;
    int progress_bar_x = center_x - progress_bar_width / 2;
    int progress_bar_y = center_y + 200;

    splash_draw_progress_bar(progress_bar_x, progress_bar_y, progress_bar_width, progress_bar_height, progress);

    return 0;
}

/*
 * Clear splash screen
 */
int splash_clear(void) {
    if (!framebuffer_is_initialized()) {
        return -1;
    }

    // Clear screen to black
    framebuffer_clear(0x00000000);
    return 0;
}

/*
 * Show splash screen with simple delay
 */
/* Removed splash_show_with_delay: callers should invoke splash_show_boot_screen directly */
