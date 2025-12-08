/*
 * SlopOS Fate/Roulette Service
 * Centralizes roulette spins, W/L accounting, and policy actions.
 */

#include "fate.h"
#include "random.h"
#include "wl_currency.h"
#include "../boot/shutdown.h"
#include "../sched/task.h"
#include <stdbool.h>

static int fate_seeded = 0;
static fate_outcome_hook_t outcome_hook = NULL;

static volatile int fate_lock = 0;

static inline void fate_lock_acquire(void) {
    while (__sync_lock_test_and_set(&fate_lock, 1)) {
        /* spin */
    }
}

static inline void fate_lock_release(void) {
    __sync_lock_release(&fate_lock);
}

static inline task_t *fate_lookup_task(uint32_t task_id) {
    task_t *task = NULL;
    if (task_get_info(task_id, &task) != 0) {
        return NULL;
    }
    return task;
}

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

int fate_set_pending(struct fate_result res, uint32_t task_id) {
    fate_lock_acquire();
    task_t *task = fate_lookup_task(task_id);
    if (!task || task_id == 0) {
        fate_lock_release();
        return -1;
    }

    /* Only one outstanding spin per task. */
    if (task->fate_pending) {
        fate_lock_release();
        return -1;
    }

    task->fate_token = res.token;
    task->fate_value = res.value;
    task->fate_pending = 1;
    fate_lock_release();
    return 0;
}

int fate_take_pending(uint32_t task_id, struct fate_result *out) {
    if (!out) {
        return -1;
    }

    fate_lock_acquire();
    task_t *task = fate_lookup_task(task_id);
    if (!task || !task->fate_pending) {
        fate_lock_release();
        return -1;
    }

    out->value = task->fate_value;
    out->token = task->fate_token;
    out->is_win = (task->fate_value & 1U) != 0;

    task->fate_token = 0;
    task->fate_value = 0;
    task->fate_pending = 0;
    fate_lock_release();
    return 0;
}

void fate_clear_pending(uint32_t task_id) {
    fate_lock_acquire();
    task_t *task = fate_lookup_task(task_id);
    if (task && task->fate_pending) {
        task->fate_token = 0;
        task->fate_value = 0;
        task->fate_pending = 0;
    }
    fate_lock_release();
}

void fate_register_outcome_hook(fate_outcome_hook_t hook) {
    outcome_hook = hook;
}

