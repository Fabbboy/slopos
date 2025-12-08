/*
 * SlopOS Kernel Roulette Visual Screen
 * The Wheel of Fate - Visual Edition
 *
 * Displays the kernel roulette spinning animation and result
 * When the wizards gamble with destiny, they deserve to SEE it happen
 */

#ifndef VIDEO_ROULETTE_H
#define VIDEO_ROULETTE_H

#include <stdint.h>

/* ========================================================================
 * ROULETTE SCREEN CONSTANTS
 * ======================================================================== */

/* Roulette colors */
#define ROULETTE_BG_COLOR       0x000000FF   /* Black background for drama */
#define ROULETTE_WHEEL_COLOR    0xFFD700FF   /* Gold wheel */
#define ROULETTE_TEXT_COLOR     0xFFFFFFFF   /* White text */
#define ROULETTE_WIN_COLOR      0x00FF00FF   /* Green for WIN */
#define ROULETTE_LOSE_COLOR     0xFF0000FF   /* Red for LOSE */
#define ROULETTE_EVEN_COLOR     0xFF4444FF   /* Red for even numbers */
#define ROULETTE_ODD_COLOR      0x44FF44FF   /* Green for odd numbers */

/* Animation settings */
#define ROULETTE_SPIN_FRAMES    30           /* Number of animation frames */
#define ROULETTE_FRAME_DELAY_MS 16           /* Delay between frames (ms) - ~60 FPS */
#define ROULETTE_RESULT_DELAY_MS 5000        /* How long to show result - 5 seconds */

#define ROULETTE_RENDER_ENABLED 1   /* Rendering is always enabled */

#endif /* VIDEO_ROULETTE_H */
