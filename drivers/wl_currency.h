/*
 * SlopOS W/L Currency System
 * The Ledger of Destiny: Tracks individual Wins (W) and Losses (L)
 *
 * Users start with 10 currency units.
 * Each W earned = +10 units, each L earned = -10 units
 * When balance reaches 0 or negative, scheduler triggers panic
 *
 * This is not a bug. This is the beating heart of SlopOS gambling addiction.
 */

#ifndef DRIVERS_WL_CURRENCY_H
#define DRIVERS_WL_CURRENCY_H

#include <stdint.h>

/* Initialize the W/L currency system (call once during early boot). */
void wl_init(void);

/* Award a win: +10 currency units (for successful operations). */
void wl_award_win(void);

/* Award a loss: -10 currency units (for recoverable errors). */
void wl_award_loss(void);

/*
 * Get current currency balance
 * Returns signed 64-bit integer (can be negative)
 */
int64_t wl_get_balance(void);

/*
 * Check if balance is acceptable
 * If balance <= 0, triggers kernel panic with disgrace
 * Called by scheduler on context switch
 */
void wl_check_balance(void);

#endif /* DRIVERS_WL_CURRENCY_H */
