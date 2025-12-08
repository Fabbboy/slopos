/*
 * SlopOS Fate/Roulette Service
 * Centralizes roulette spins, win/loss accounting, and outcome handling.
 */

#ifndef DRIVERS_FATE_H
#define DRIVERS_FATE_H

#include <stdint.h>

struct fate_result {
    uint32_t value;  /* Raw fate number */
    int is_win;      /* 1 if odd (win), 0 if even (loss) */
};

enum fate_resolution {
    FATE_RESOLUTION_NONE = 0,
    FATE_RESOLUTION_REBOOT_ON_LOSS,
};

/* Ensure RNG seeding is performed once. Safe to call multiple times. */
void fate_init(void);

/* Spin the wheel of fate (no side effects besides RNG). */
struct fate_result fate_spin(void);

/* Apply W/L accounting and optional resolution policy (e.g., reboot on loss). */
void fate_apply_outcome(const struct fate_result *res, enum fate_resolution resolution);

/* Pending fate helpers for syscall/user handshake. */
void fate_set_pending(struct fate_result res);
int fate_take_pending(struct fate_result *out);
void fate_clear_pending(void);

#endif /* DRIVERS_FATE_H */

