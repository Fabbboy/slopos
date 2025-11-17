/*
 * SlopOS Kernel Roulette Visual Screen Implementation
 * The Wheel of Fate - Now with 100% more visual gambling addiction
 */

#include <stdint.h>
#include <stddef.h>
#include "roulette.h"
#include "framebuffer.h"
#include "graphics.h"
#include "font.h"
#include "../drivers/serial.h"
#include "../drivers/pit.h"

/* ========================================================================
 * ANIMATION HELPERS
 * ======================================================================== */

/*
 * Delay function for roulette animations
 * Uses busy-wait optimized for QEMU timing
 */
static void roulette_delay_ms(uint32_t milliseconds) {
    volatile uint64_t cycles_per_ms = 50000; // Calibrated for QEMU

    for (uint32_t ms = 0; ms < milliseconds; ms++) {
        for (volatile uint64_t i = 0; i < cycles_per_ms; i++) {
            __asm__ volatile ("nop");
        }
    }
}

/* ========================================================================
 * WHEEL DRAWING FUNCTIONS
 * ======================================================================== */

/*
 * Draw a roulette wheel at specified rotation angle
 * center_x, center_y: wheel center position
 * radius: wheel radius
 * angle: rotation angle (0-360 degrees)
 * fate_number: the number we're spinning toward
 */
static void draw_roulette_wheel(int center_x, int center_y, int radius, int angle, uint32_t fate_number) {
    // Draw outer wheel circle (gold)
    graphics_draw_circle_filled(center_x, center_y, radius, ROULETTE_WHEEL_COLOR);

    // Draw inner black circle
    graphics_draw_circle_filled(center_x, center_y, radius - 20, 0x000000FF);

    // Draw 12 segments around the wheel (like a clock)
    for (int i = 0; i < 12; i++) {
        int seg_angle = (i * 30 + angle) % 360;

        // Calculate segment color (alternating red/green for even/odd)
        uint32_t seg_color = (i % 2) ? ROULETTE_EVEN_COLOR : ROULETTE_ODD_COLOR;

        // Draw segment line from center outward
        // Simplified: just draw radial lines
        int dx = (int)((radius - 10) * 0.866); // cos approximation
        int dy = (int)((radius - 10) * 0.5);   // sin approximation

        // Rotate based on segment
        int x_end = center_x;
        int y_end = center_y - (radius - 10);

        // Simple rotation for visual effect
        if (seg_angle >= 45 && seg_angle < 135) {
            x_end = center_x + (radius - 10);
            y_end = center_y;
        } else if (seg_angle >= 135 && seg_angle < 225) {
            x_end = center_x;
            y_end = center_y + (radius - 10);
        } else if (seg_angle >= 225 && seg_angle < 315) {
            x_end = center_x - (radius - 10);
            y_end = center_y;
        }

        graphics_draw_line(center_x, center_y, x_end, y_end, seg_color);
    }

    // Draw center circle with spinning indicator
    graphics_draw_circle_filled(center_x, center_y, 30, ROULETTE_TEXT_COLOR);
    graphics_draw_circle_filled(center_x, center_y, 25, 0x000000FF);

    // Draw pointer at top (always points up)
    graphics_draw_triangle(
        center_x, center_y - radius - 30,           // Top point
        center_x - 20, center_y - radius - 10,      // Left point
        center_x + 20, center_y - radius - 10,      // Right point
        ROULETTE_TEXT_COLOR
    );
}

/*
 * Draw the fate number display
 */
static void draw_fate_number(int center_x, int y_pos, uint32_t fate_number, int revealed) {
    if (!revealed) {
        // Draw mystery box
        graphics_draw_rect_filled(center_x - 100, y_pos, 200, 60, 0x333333FF);
        graphics_draw_rect(center_x - 100, y_pos, 200, 60, ROULETTE_WHEEL_COLOR);
        font_draw_string(center_x - 40, y_pos + 20, "? ? ?", ROULETTE_TEXT_COLOR, 0x00000000);
    } else {
        // Draw number box
        uint32_t box_color = (fate_number & 1) ? ROULETTE_ODD_COLOR : ROULETTE_EVEN_COLOR;
        graphics_draw_rect_filled(center_x - 100, y_pos, 200, 60, box_color);
        graphics_draw_rect(center_x - 100, y_pos, 200, 60, ROULETTE_WHEEL_COLOR);

        // Draw the fate number
        char num_str[20];
        int pos = 0;
        uint32_t temp = fate_number;

        // Convert to string
        if (temp == 0) {
            num_str[pos++] = '0';
        } else {
            char digits[20];
            int digit_count = 0;
            while (temp > 0) {
                digits[digit_count++] = '0' + (temp % 10);
                temp /= 10;
            }
            // Reverse digits
            for (int i = digit_count - 1; i >= 0; i--) {
                num_str[pos++] = digits[i];
            }
        }
        num_str[pos] = '\0';

        // Center the number
        int text_x = center_x - (pos * 8) / 2;
        font_draw_string(text_x, y_pos + 20, num_str, 0x000000FF, 0x00000000);
    }
}

/*
 * Draw result banner (WIN or LOSE)
 */
static void draw_result_banner(int center_x, int y_pos, uint32_t fate_number) {
    const char *result_text;
    const char *sub_text;
    uint32_t banner_color;

    if (fate_number & 1) {
        // ODD = WIN
        result_text = "W I N !";
        sub_text = "Fortune smiles upon the slop!";
        banner_color = ROULETTE_WIN_COLOR;
    } else {
        // EVEN = LOSE
        result_text = "L O S E";
        sub_text = "L bozzo lol - try again!";
        banner_color = ROULETTE_LOSE_COLOR;
    }

    // Draw result banner
    graphics_draw_rect_filled(center_x - 200, y_pos, 400, 80, banner_color);
    graphics_draw_rect(center_x - 202, y_pos - 2, 404, 84, ROULETTE_WHEEL_COLOR);

    // Draw text (centered)
    int text_x = center_x - 60;
    font_draw_string(text_x, y_pos + 15, result_text, 0x000000FF, 0x00000000);

    // Draw subtext
    int sub_x = center_x - 140;
    font_draw_string(sub_x, y_pos + 50, sub_text, 0x000000FF, 0x00000000);
}

/* ========================================================================
 * MAIN ROULETTE SCREEN FUNCTION
 * ======================================================================== */

/*
 * Show the full roulette spinning animation and result
 */
int roulette_show_spin(uint32_t fate_number) {
    if (!framebuffer_is_initialized()) {
        kprintln("ROULETTE: Framebuffer not available, using fallback");
        roulette_show_spin_fallback(fate_number);
        return -1;
    }

    kprintln("ROULETTE: Displaying visual wheel of fate...");

    // Get screen dimensions
    uint32_t width = framebuffer_get_width();
    uint32_t height = framebuffer_get_height();
    int center_x = width / 2;
    int center_y = height / 2;

    // Clear screen to dramatic black
    framebuffer_clear(ROULETTE_BG_COLOR);

    // Draw title
    font_draw_string(center_x - 150, 50, "=== THE WHEEL OF FATE ===", ROULETTE_WHEEL_COLOR, 0x00000000);
    font_draw_string(center_x - 100, 80, "Spinning destiny...", ROULETTE_TEXT_COLOR, 0x00000000);

    // Initial delay for dramatic effect
    roulette_delay_ms(500);

    // ANIMATION PHASE 1: Fast spinning
    kprintln("ROULETTE: Phase 1 - Fast spin");
    for (int frame = 0; frame < 15; frame++) {
        // Clear wheel area
        graphics_draw_rect_filled(center_x - 200, center_y - 200, 400, 400, ROULETTE_BG_COLOR);

        // Draw spinning wheel
        int angle = (frame * 30) % 360;
        draw_roulette_wheel(center_x, center_y, 120, angle, fate_number);

        // Draw unrevealed number
        draw_fate_number(center_x, center_y + 180, fate_number, 0);

        roulette_delay_ms(ROULETTE_FRAME_DELAY_MS);
    }

    // ANIMATION PHASE 2: Slowing down
    kprintln("ROULETTE: Phase 2 - Slowing down");
    for (int frame = 0; frame < 10; frame++) {
        // Clear wheel area
        graphics_draw_rect_filled(center_x - 200, center_y - 200, 400, 400, ROULETTE_BG_COLOR);

        // Draw spinning wheel (slower)
        int angle = (frame * 20) % 360;
        draw_roulette_wheel(center_x, center_y, 120, angle, fate_number);

        // Draw unrevealed number
        draw_fate_number(center_x, center_y + 180, fate_number, 0);

        roulette_delay_ms(ROULETTE_FRAME_DELAY_MS + frame * 10); // Progressively slower
    }

    // ANIMATION PHASE 3: Final wobble and stop
    kprintln("ROULETTE: Phase 3 - Final wobble");
    for (int frame = 0; frame < 5; frame++) {
        // Clear wheel area
        graphics_draw_rect_filled(center_x - 200, center_y - 200, 400, 400, ROULETTE_BG_COLOR);

        // Draw spinning wheel (very slow wobble)
        int angle = (frame * 5) % 360;
        draw_roulette_wheel(center_x, center_y, 120, angle, fate_number);

        // Draw unrevealed number
        draw_fate_number(center_x, center_y + 180, fate_number, 0);

        roulette_delay_ms(ROULETTE_FRAME_DELAY_MS + 100);
    }

    // REVEAL PHASE: Show the number
    kprintln("ROULETTE: Revealing fate number...");

    // Clear wheel area
    graphics_draw_rect_filled(center_x - 200, center_y - 200, 400, 400, ROULETTE_BG_COLOR);

    // Draw final wheel position
    draw_roulette_wheel(center_x, center_y, 120, 0, fate_number);

    // Reveal the number with flash effect
    for (int flash = 0; flash < 3; flash++) {
        draw_fate_number(center_x, center_y + 180, fate_number, 1);
        roulette_delay_ms(100);

        if (flash < 2) {
            graphics_draw_rect_filled(center_x - 100, center_y + 180, 200, 60, ROULETTE_BG_COLOR);
            roulette_delay_ms(100);
        }
    }

    // Final number display
    draw_fate_number(center_x, center_y + 180, fate_number, 1);

    roulette_delay_ms(500);

    // RESULT PHASE: Show WIN or LOSE
    kprintln("ROULETTE: Displaying result...");

    // Draw result banner
    draw_result_banner(center_x, center_y + 270, fate_number);

    // Show W/L currency effect
    const char *currency_text;
    if (fate_number & 1) {
        currency_text = "+10 W's (currency units)";
    } else {
        currency_text = "-10 W's (currency units)";
    }
    font_draw_string(center_x - 100, center_y + 370, currency_text, ROULETTE_TEXT_COLOR, 0x00000000);

    // Display result for dramatic effect
    roulette_delay_ms(ROULETTE_RESULT_DELAY_MS);

    kprintln("ROULETTE: Wheel of fate complete");

    return 0;
}

/*
 * Fallback roulette display for when framebuffer is not available
 */
void roulette_show_spin_fallback(uint32_t fate_number) {
    kprintln("ROULETTE: Using text-only fallback display");
    kprintln("");
    kprintln("========================================");
    kprintln("    THE WHEEL OF FATE IS SPINNING     ");
    kprintln("========================================");
    kprintln("");

    // Simple text animation
    for (int i = 0; i < 5; i++) {
        kprint(".");
        roulette_delay_ms(200);
    }
    kprintln("");

    kprintln("");
    kprint("Fate number: ");
    kprint_dec(fate_number);
    kprintln("");

    if (fate_number & 1) {
        kprintln("");
        kprintln("========================================");
        kprintln("           W I N !                      ");
        kprintln("    Fortune smiles upon the slop!      ");
        kprintln("========================================");
    } else {
        kprintln("");
        kprintln("========================================");
        kprintln("           L O S E                      ");
        kprintln("      L bozzo lol - try again!         ");
        kprintln("========================================");
    }

    kprintln("");
    roulette_delay_ms(1000);
}
