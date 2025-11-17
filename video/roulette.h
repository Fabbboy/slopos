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
#define ROULETTE_FRAME_DELAY_MS 150          /* Delay between frames (ms) - MUCH SLOWER */
#define ROULETTE_RESULT_DELAY_MS 5000        /* How long to show result - 5 seconds */

/* ========================================================================
 * ROULETTE SCREEN FUNCTIONS
 * ======================================================================== */

/*
 * Display the kernel roulette screen with spinning animation
 *
 * fate_number: The random number that determines destiny
 *
 * This function:
 * 1. Clears screen to dramatic black
 * 2. Shows "SPINNING THE WHEEL OF FATE" title
 * 3. Animates a spinning wheel
 * 4. Reveals the fate number
 * 5. Shows WIN (odd) or LOSE (even) result
 *
 * Returns 0 on success, negative on error
 */
int roulette_show_spin(uint32_t fate_number);

/*
 * Quick version for when framebuffer is not available
 * Falls back to serial-only output
 */
void roulette_show_spin_fallback(uint32_t fate_number);

#endif /* VIDEO_ROULETTE_H */
