/*
 * SlopOS Fate/Roulette Service
 * Centralizes roulette spins, W/L accounting, and policy actions.
 */

#include "fate.h"
#include "random.h"
#include "wl_currency.h"
#include "../boot/shutdown.h"
#include <stdbool.h>

static int fate_seeded = 0;
static struct fate_result pending_fate = {0};
static int pending_valid = 0;
static fate_outcome_hook_t outcome_hook = NULL;

static uint32_t fate_next_token(void) {
    /* Token must be non-zero to distinguish from uninitialized/default values. */
    uint32_t token = 0;
    while (token == 0) {
        token = random_next();
    }
    return token;
}

void fate_init(void) {
    if (fate_seeded) {
        return;
    }
    random_init();
    fate_seeded = 1;
}

struct fate_result fate_spin(void) {
    fate_init();
    uint32_t value = random_next();
    struct fate_result res = {
        .value = value,
        .token = fate_next_token(),
        .is_win = (value & 1U) != 0,
    };
    return res;
}

void fate_apply_outcome(const struct fate_result *res,
                        enum fate_resolution resolution,
                        bool notify_hook) {
    if (!res) {
        return;
    }

    if (res->is_win) {
        wl_award_win();
    } else {
        wl_award_loss();
        if (resolution == FATE_RESOLUTION_REBOOT_ON_LOSS) {
            kernel_reboot("Roulette loss - spinning again");
        }
    }

    if (notify_hook && outcome_hook) {
        outcome_hook(res);
    }
}

void fate_set_pending(struct fate_result res) {
    pending_fate = res;
    pending_valid = 1;
}

int fate_take_pending(struct fate_result *out) {
    if (!out || !pending_valid) {
        return -1;
    }
    *out = pending_fate;
    pending_valid = 0;
    return 0;
}

void fate_clear_pending(void) {
    pending_valid = 0;
}

void fate_register_outcome_hook(fate_outcome_hook_t hook) {
    outcome_hook = hook;
}

