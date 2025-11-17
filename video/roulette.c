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
#include "splash.h"
#include "../drivers/serial.h"
#include "../drivers/pit.h"

/* ========================================================================
 * ANIMATION HELPERS
 * ======================================================================== */

/*
 * Delay function for roulette animations
 * Uses busy-wait optimized for QEMU timing
 * MUCH SLOWER for visibility
 */
static void roulette_delay_ms(uint32_t milliseconds) {
    volatile uint64_t cycles_per_ms = 150000; // 3x slower for better visibility

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
 * Draw a roulette wheel at specified rotation angle - MAXIMUM VISIBILITY
 * center_x, center_y: wheel center position
 * radius: wheel radius
 * angle: rotation angle (0-360 degrees)
 * fate_number: the number we're spinning toward
 */
static void draw_roulette_wheel(int center_x, int center_y, int radius, int angle, uint32_t fate_number) {
    // Draw BLACK background circle
    graphics_draw_circle_filled(center_x, center_y, radius + 5, 0x000000FF);

    // Draw 8 segments as FILLED BOXES radiating from center
    // This is much simpler and more visible than wedges
    for (int i = 0; i < 8; i++) {
        int base_angle = (i * 45 + angle) % 360;
        int octant = (base_angle / 45) % 8;

        // Alternate PURE RED and PURE GREEN
        uint32_t color = (i % 2 == 0) ? 0xFF0000FF : 0x00FF00FF;

        // Draw FILLED rectangular segments radiating outward
        // Much more visible than triangular wedges
        for (int r = 15; r < radius; r++) {
            int x1, y1, x2, y2;

            // Calculate segment boundaries
            switch (octant) {
                case 0: // North
                    x1 = center_x - 20; y1 = center_y - r;
                    x2 = center_x + 20; y2 = center_y - r;
                    break;
                case 1: // NE
                    x1 = center_x + r * 6 / 10; y1 = center_y - r * 6 / 10;
                    x2 = center_x + r * 8 / 10; y2 = center_y - r * 4 / 10;
                    break;
                case 2: // East
                    x1 = center_x + r; y1 = center_y - 20;
                    x2 = center_x + r; y2 = center_y + 20;
                    break;
                case 3: // SE
                    x1 = center_x + r * 6 / 10; y1 = center_y + r * 6 / 10;
                    x2 = center_x + r * 4 / 10; y2 = center_y + r * 8 / 10;
                    break;
                case 4: // South
                    x1 = center_x - 20; y1 = center_y + r;
                    x2 = center_x + 20; y2 = center_y + r;
                    break;
                case 5: // SW
                    x1 = center_x - r * 6 / 10; y1 = center_y + r * 6 / 10;
                    x2 = center_x - r * 8 / 10; y2 = center_y + r * 4 / 10;
                    break;
                case 6: // West
                    x1 = center_x - r; y1 = center_y - 20;
                    x2 = center_x - r; y2 = center_y + 20;
                    break;
                case 7: // NW
                    x1 = center_x - r * 6 / 10; y1 = center_y - r * 6 / 10;
                    x2 = center_x - r * 4 / 10; y2 = center_y - r * 8 / 10;
                    break;
                default:
                    x1 = center_x; y1 = center_y;
                    x2 = center_x; y2 = center_y;
                    break;
            }

            // Draw THICK horizontal line for this radius
            graphics_draw_line(x1, y1, x2, y2, color);
        }
    }

    // Draw WHITE dividing lines between segments (VERY THICK)
    for (int i = 0; i < 8; i++) {
        int base_angle = (i * 45 + angle) % 360;
        int octant = (base_angle / 45) % 8;
        int x_end, y_end;

        switch (octant) {
            case 0: x_end = center_x; y_end = center_y - radius; break;
            case 1: x_end = center_x + radius * 7 / 10; y_end = center_y - radius * 7 / 10; break;
            case 2: x_end = center_x + radius; y_end = center_y; break;
            case 3: x_end = center_x + radius * 7 / 10; y_end = center_y + radius * 7 / 10; break;
            case 4: x_end = center_x; y_end = center_y + radius; break;
            case 5: x_end = center_x - radius * 7 / 10; y_end = center_y + radius * 7 / 10; break;
            case 6: x_end = center_x - radius; y_end = center_y; break;
            case 7: x_end = center_x - radius * 7 / 10; y_end = center_y - radius * 7 / 10; break;
            default: x_end = center_x; y_end = center_y; break;
        }

        // Draw ULTRA THICK white dividing lines
        for (int thick = -5; thick <= 5; thick++) {
            graphics_draw_line(center_x, center_y, x_end + thick, y_end, 0xFFFFFFFF);
            graphics_draw_line(center_x, center_y, x_end, y_end + thick, 0xFFFFFFFF);
        }
    }

    // Draw center circle (GOLD with BLACK inner) - much larger
    graphics_draw_circle_filled(center_x, center_y, 50, 0xFFD700FF);  // Gold
    graphics_draw_circle_filled(center_x, center_y, 42, 0x000000FF);  // Black center

    // Draw HUGE pointer at top (filled yellow triangle)
    int pointer_top = center_y - radius - 60;
    int pointer_base = center_y - radius - 20;

    for (int y = pointer_top; y < pointer_base; y++) {
        int width = ((y - pointer_top) * 80) / 40;
        graphics_draw_hline(center_x - width/2, center_x + width/2, y, 0xFFFF00FF);
    }

    // Draw pointer border
    graphics_draw_line(center_x, pointer_top, center_x - 40, pointer_base, 0xFFFFFFFF);
    graphics_draw_line(center_x, pointer_top, center_x + 40, pointer_base, 0xFFFFFFFF);
    graphics_draw_hline(center_x - 40, center_x + 40, pointer_base, 0xFFFFFFFF);
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
    roulette_delay_ms(1000);

    // ANIMATION PHASE 1: Fast spinning (more frames, slower)
    kprintln("ROULETTE: Phase 1 - Fast spin");
    for (int frame = 0; frame < 24; frame++) {
        // Clear wheel area
        graphics_draw_rect_filled(center_x - 200, center_y - 200, 400, 400, ROULETTE_BG_COLOR);

        // Draw spinning wheel - rotate through all 8 segments
        int angle = (frame * 45) % 360;  // 45 degrees per frame = full rotation every 8 frames
        draw_roulette_wheel(center_x, center_y, 120, angle, fate_number);

        // Draw unrevealed number
        draw_fate_number(center_x, center_y + 180, fate_number, 0);

        roulette_delay_ms(ROULETTE_FRAME_DELAY_MS);
    }

    // ANIMATION PHASE 2: Slowing down (more frames, progressive slowdown)
    kprintln("ROULETTE: Phase 2 - Slowing down");
    for (int frame = 0; frame < 16; frame++) {
        // Clear wheel area
        graphics_draw_rect_filled(center_x - 200, center_y - 200, 400, 400, ROULETTE_BG_COLOR);

        // Draw spinning wheel (slower rotation)
        int angle = (frame * 22) % 360;  // Slower rotation
        draw_roulette_wheel(center_x, center_y, 120, angle, fate_number);

        // Draw unrevealed number
        draw_fate_number(center_x, center_y + 180, fate_number, 0);

        roulette_delay_ms(ROULETTE_FRAME_DELAY_MS + frame * 20); // Each frame gets slower
    }

    // ANIMATION PHASE 3: Final wobble and stop
    kprintln("ROULETTE: Phase 3 - Final wobble");
    for (int frame = 0; frame < 8; frame++) {
        // Clear wheel area
        graphics_draw_rect_filled(center_x - 200, center_y - 200, 400, 400, ROULETTE_BG_COLOR);

        // Draw spinning wheel (very slow wobble back and forth)
        int wobble_angle = (frame % 2) ? 10 : -10;  // Wobble left and right
        draw_roulette_wheel(center_x, center_y, 120, wobble_angle, fate_number);

        // Draw unrevealed number
        draw_fate_number(center_x, center_y + 180, fate_number, 0);

        roulette_delay_ms(ROULETTE_FRAME_DELAY_MS + 200);
    }

    // REVEAL PHASE: Show the number
    kprintln("ROULETTE: Revealing fate number...");

    // Clear wheel area
    graphics_draw_rect_filled(center_x - 200, center_y - 200, 400, 400, ROULETTE_BG_COLOR);

    // Draw final wheel position
    draw_roulette_wheel(center_x, center_y, 120, 0, fate_number);

    // Longer pause before reveal
    roulette_delay_ms(800);

    // Reveal the number with SLOWER flash effect
    for (int flash = 0; flash < 5; flash++) {
        draw_fate_number(center_x, center_y + 180, fate_number, 1);
        roulette_delay_ms(250);  // Slower flashes

        if (flash < 4) {
            graphics_draw_rect_filled(center_x - 100, center_y + 180, 200, 60, ROULETTE_BG_COLOR);
            roulette_delay_ms(250);
        }
    }

    // Final number display
    draw_fate_number(center_x, center_y + 180, fate_number, 1);

    roulette_delay_ms(1000);  // Longer pause after number reveal

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

    // If LOSE, add instruction to reset
    if ((fate_number & 1) == 0) {
        font_draw_string(center_x - 120, center_y + 410, "Press RESET to try again...", 0xFFFF00FF, 0x00000000);
    } else {
        font_draw_string(center_x - 120, center_y + 410, "Continuing to OS...", 0x00FF00FF, 0x00000000);
    }

    // Display result for dramatic effect
    roulette_delay_ms(ROULETTE_RESULT_DELAY_MS);

    kprintln("ROULETTE: Wheel of fate complete");

    // If WIN, clear the screen and show transition message before returning to OS
    if (fate_number & 1) {
        // Clear to dark blue background for transition
        framebuffer_clear(0x001122FF);

        // Show simple transition message
        uint32_t width = framebuffer_get_width();
        uint32_t height = framebuffer_get_height();
        int msg_x = width / 2 - 150;
        int msg_y = height / 2 - 20;

        font_draw_string(msg_x, msg_y, "You won! Continuing to SlopOS...", 0xFFFFFFFF, 0x00000000);

        // Brief pause before OS takes over
        roulette_delay_ms(1000);

        // Restore the normal post-boot graphics demo screen
        splash_draw_graphics_demo();

        kprintln("ROULETTE: Graphics demo restored, returning to OS");
    }

    // Return 0 for WIN (odd), 1 for LOSE (even) so caller knows what happened
    return (fate_number & 1) ? 0 : 1;
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
