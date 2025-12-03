/*
 * SlopOS W/L Currency System Implementation
 * The Ledger of Destiny: Every win and loss recorded in kernel memory
 *
 * Start with 10 currency. Each W = +1, each L = -1.
 * Reach 0 or below and the scheduler ends you.
 */

#include "wl_currency.h"
#include "../drivers/serial.h"
#include "../lib/numfmt.h"

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
    char buffer[24];
    if (numfmt_i64_to_decimal(value, buffer, sizeof(buffer)) == 0) {
        buffer[0] = '0';
        buffer[1] = '\0';
    }
    serial_emergency_puts(buffer);
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
