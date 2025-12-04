/*
 * SlopOS Splash Screen Header
 * Interface for boot splash screen display
 */

#ifndef VIDEO_SPLASH_H
#define VIDEO_SPLASH_H

#include <stdint.h>

/* ========================================================================
 * SPLASH SCREEN CONSTANTS
 * ======================================================================== */

/* Splash screen colors */
#define SPLASH_BG_COLOR         0x001122FF   /* Dark blue background */
#define SPLASH_LOGO_COLOR       0xFFFFFFFF   /* White logo */
#define SPLASH_TEXT_COLOR       0xFFFFFFFF   /* White text */
#define SPLASH_ACCENT_COLOR     0x00AAFF    /* Light blue accent */
#define SPLASH_PROGRESS_COLOR   0x00FF88FF   /* Green progress bar */

/* Splash screen timing */
#define SPLASH_DISPLAY_TIME_MS  2000         /* 2 seconds */

/* ========================================================================
 * SPLASH SCREEN FUNCTIONS
 * ======================================================================== */

/*
 * Display boot splash screen
 * Shows SlopOS logo, version, and loading animation
 * Returns 0 on success, negative on error
 */
int splash_show_boot_screen(void);

/*
 * Update splash screen with loading progress
 * progress: 0-100 percentage
 * message: current loading stage message
 */
int splash_update_progress(int progress, const char *message);

/*
 * Clear splash screen and prepare for normal graphics
 */
int splash_clear(void);

/*
 * Report actual boot progress (called during kernel initialization)
 * progress: 0-100 percentage of boot completion
 * message: current initialization stage message
 */
int splash_report_progress(int progress, const char *message);

/*
 * Mark splash screen as complete
 */
int splash_finish(void);

/*
 * Draw the post-boot graphics demo screen
 * Shows colored rectangles, circle, border, and welcome text
 */
int splash_draw_graphics_demo(void);

#endif /* VIDEO_SPLASH_H */
