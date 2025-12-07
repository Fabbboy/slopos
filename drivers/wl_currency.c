/*
 * SlopOS W/L Currency System Implementation
 * The Ledger of Destiny: Every win and loss recorded in kernel memory
 *
 * Start with 10 currency. Each W = +10, each L = -10.
 * Reach 0 or below and the scheduler ends you.
 */

#include "wl_currency.h"
#include "../drivers/serial.h"
#include "../lib/numfmt.h"
#include "../boot/kernel_panic.h"

/* ========================================================================
 * GLOBAL W/L STATE
 * ======================================================================== */

#define WL_STARTING_BALANCE 10
#define WL_UNIT_DELTA       10

static int64_t w_balance = WL_STARTING_BALANCE;  /* Start with 10 currency */
static int w_initialized = 0;

/* ========================================================================
 * INITIALIZATION
 * ======================================================================== */

void wl_init(void) {
    if (w_initialized) {
        return;
    }

    w_balance = WL_STARTING_BALANCE;
    w_initialized = 1;
}

/* ========================================================================
 * AWARD FUNCTIONS
 * ======================================================================== */

void wl_award_win(void) {
    if (!w_initialized) {
        serial_emergency_puts("[WL] award_win before wl_init\n");
        return;
    }

    w_balance += WL_UNIT_DELTA;
}

void wl_award_loss(void) {
    if (!w_initialized) {
        serial_emergency_puts("[WL] award_loss before wl_init\n");
        return;
    }

    w_balance -= WL_UNIT_DELTA;
}

/* ========================================================================
 * QUERY FUNCTIONS
 * ======================================================================== */

int64_t wl_get_balance(void) {
    if (!w_initialized) {
        return WL_STARTING_BALANCE;
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
    if (!w_initialized) {
        return;
    }

    if (w_balance <= 0) {
        serial_emergency_puts("\n=== W/L CURRENCY CHECK FAILED ===\n");
        serial_emergency_puts("User has depleted all currency. Current balance: ");
        wl_output_decimal(w_balance);
        serial_emergency_puts("\n");
        serial_emergency_puts("The scheduler has no mercy. Your gambling addiction bankrupted you.\n");
        serial_emergency_puts("[WL] User currency critical - initiating disgrace protocol\n");

        kernel_panic("[WL] Zero or negative currency balance - the house always wins, skill issue lol");
    }
}
