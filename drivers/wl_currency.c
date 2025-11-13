/*
 * SlopOS W/L Currency System Implementation
 * The Ledger of Destiny: Every win and loss recorded in kernel memory
 *
 * Start with 10 currency. Each W = +1, each L = -1.
 * Reach 0 or below and the scheduler ends you.
 *
 * SCHLOTER PROTOCOL EXTENSIONS:
 * Michael Schloter, the Meme Sorcerer of Mönchengladbach, enhanced this system
 * with "Maximale Schlotterung" (Maximum Shaking) - multipliers and Island Mode
 * for truly chaotic gambling experiences.
 */

#include "wl_currency.h"
#include "../drivers/serial.h"
#include "../drivers/random.h"

/* ========================================================================
 * GLOBAL W/L STATE
 * ======================================================================== */

static int64_t w_balance = 10;  /* Start with 10 currency */
static int is_initialized = 0;

/* ========================================================================
 * INITIALIZATION
 * ======================================================================== */

void wl_init(void) {
    if (is_initialized) {
        return;
    }

    w_balance = 10;  /* Reset to starting balance */
    is_initialized = 1;
}

/* ========================================================================
 * AWARD FUNCTIONS
 * ======================================================================== */

void take_w(void) {
    if (!is_initialized) {
        wl_init();
    }

    w_balance += 1;
}

void take_l(void) {
    if (!is_initialized) {
        wl_init();
    }

    w_balance -= 1;
}

/* ========================================================================
 * QUERY FUNCTIONS
 * ======================================================================== */

int64_t wl_get_balance(void) {
    if (!is_initialized) {
        wl_init();
    }

    return w_balance;
}

/* ========================================================================
 * HELPER: Output 64-bit signed decimal
 * ======================================================================== */

static void wl_output_decimal(int64_t value) {
    if (value < 0) {
        serial_emergency_puts("-");
        value = -value;
    }

    if (value == 0) {
        serial_emergency_putc('0');
        return;
    }

    char buffer[20];
    int pos = 0;

    while (value > 0) {
        buffer[pos++] = '0' + (value % 10);
        value /= 10;
    }

    while (pos > 0) {
        serial_emergency_putc(buffer[--pos]);
    }
}

/* ========================================================================
 * BALANCE CHECK
 * ======================================================================== */

void wl_check_balance(void) {
    if (!is_initialized) {
        return;
    }

    if (w_balance <= 0) {
        serial_emergency_puts("\n=== W/L CURRENCY CHECK FAILED ===\n");
        serial_emergency_puts("User has depleted all currency. Current balance: ");
        wl_output_decimal(w_balance);
        serial_emergency_puts("\n");
        serial_emergency_puts("The scheduler has no mercy. Your gambling addiction bankrupted you.\n");
        serial_emergency_puts("[WL] User currency critical - initiating disgrace protocol\n");

        /* Trigger panic through extern - declared in integration.h */
        extern void kernel_panic(const char *message);
        kernel_panic("[WL] Zero or negative currency balance - the house always wins, skill issue lol");
    }
}

/* ========================================================================
 * SCHLOTER PROTOCOL - GAMBLING ENHANCEMENTS
 * Implemented by Michael Schloter, Meme Sorcerer of Mönchengladbach
 * ======================================================================== */

/*
 * Schloter Multiplier: Award multiple wins
 * "When fortune favors the bold, let it rain currency!"
 */
void schloter_multi_w(int multiplier) {
    if (!is_initialized) {
        wl_init();
    }

    if (multiplier < 0) {
        multiplier = 0;
    }

    w_balance += multiplier;

    serial_emergency_puts("[SCHLOTER] Multi-W activated! Multiplier: ");
    wl_output_decimal(multiplier);
    serial_emergency_puts(" | New balance: ");
    wl_output_decimal(w_balance);
    serial_emergency_puts("\n");
}

/*
 * Schloter Multiplier: Award multiple losses
 * "Chaos demands its payment. Maximum Shaking incoming!"
 */
void schloter_multi_l(int multiplier) {
    if (!is_initialized) {
        wl_init();
    }

    if (multiplier < 0) {
        multiplier = 0;
    }

    w_balance -= multiplier;

    serial_emergency_puts("[SCHLOTER] Multi-L activated! Multiplier: ");
    wl_output_decimal(multiplier);
    serial_emergency_puts(" | New balance: ");
    wl_output_decimal(w_balance);
    serial_emergency_puts("\n");
}

/*
 * Island Mode: The signature Schloter gambling experience
 *
 * The wheel spins THREE times. Each spin decides a W or L.
 * The net result is applied to the balance.
 *
 * This is "Maximale Schlotterung" (Maximum Shaking) in action:
 * - Unpredictable
 * - Chaotic
 * - Magnificently absurd
 *
 * Returns: Net W/L change (positive = wins, negative = losses)
 */
int schloter_island_mode(void) {
    if (!is_initialized) {
        wl_init();
    }

    serial_emergency_puts("\n=== SCHLOTER ISLAND MODE ACTIVATED ===\n");
    serial_emergency_puts("Spinning the wheel THREE times...\n");
    serial_emergency_puts("Maximum Shaking in progress!\n\n");

    int net_result = 0;

    for (int i = 1; i <= 3; i++) {
        uint32_t spin = random_next();
        int is_win = (spin % 2 == 1); // Odd = win, even = loss

        serial_emergency_puts("Spin ");
        wl_output_decimal(i);
        serial_emergency_puts(": 0x");

        // Output spin value in hex
        char hex_buffer[9];
        for (int j = 7; j >= 0; j--) {
            uint8_t nibble = (spin >> (j * 4)) & 0xF;
            hex_buffer[7 - j] = (nibble < 10) ? ('0' + nibble) : ('A' + nibble - 10);
        }
        hex_buffer[8] = '\0';
        serial_emergency_puts(hex_buffer);

        if (is_win) {
            serial_emergency_puts(" (ODD) - WIN!\n");
            net_result++;
            w_balance++;
        } else {
            serial_emergency_puts(" (EVEN) - LOSS!\n");
            net_result--;
            w_balance--;
        }
    }

    serial_emergency_puts("\n=== ISLAND MODE COMPLETE ===\n");
    serial_emergency_puts("Net result: ");
    if (net_result > 0) {
        serial_emergency_puts("+");
    }
    wl_output_decimal(net_result);
    serial_emergency_puts(" | Final balance: ");
    wl_output_decimal(w_balance);
    serial_emergency_puts("\n");

    if (net_result > 0) {
        serial_emergency_puts("The Meme Sorcerer smiles upon you.\n");
    } else if (net_result < 0) {
        serial_emergency_puts("Mönchengladbach demands its tribute.\n");
    } else {
        serial_emergency_puts("Perfect balance. Schlotercore achieved.\n");
    }

    serial_emergency_puts("================================\n\n");

    return net_result;
}
