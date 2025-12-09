/*
 * Userland bootstrap and task launch orchestration.
 * Registers roulette outcome hooks and starts user-mode programs in order.
 */

#include "../boot/init.h"
#include "../drivers/fate.h"
#include "../drivers/wl_currency.h"
#include "../lib/klog.h"
#include "../sched/task.h"
#include "../sched/scheduler.h"
#include "../user/loader.h"
#include "../user/shell_user.h"

#include <stdint.h>

/* Forward declarations for user-mode entry points. */
extern void roulette_user_main(void *arg);

/* Simple helper to spawn and schedule a user task with basic logging/WL. */
static int userland_spawn_and_schedule(const char *name, task_entry_t entry, uint8_t priority) {
    uint32_t task_id = user_spawn_program(name, entry, NULL, priority);
    if (task_id == INVALID_TASK_ID) {
        klog_printf(KLOG_INFO, "USERLAND: Failed to create task '%s'\n", name);
        wl_award_loss();
        return -1;
    }

    task_t *task_info;
    if (task_get_info(task_id, &task_info) != 0) {
        klog_printf(KLOG_INFO, "USERLAND: Failed to fetch task info for '%s'\n", name);
        wl_award_loss();
        return -1;
    }

    if (schedule_task(task_info) != 0) {
        klog_printf(KLOG_INFO, "USERLAND: Failed to schedule task '%s'\n", name);
        wl_award_loss();
        task_terminate(task_id);
        return -1;
    }

    wl_award_win();
    return 0;
}

/* Shell launch (one-shot) */
static int shell_spawned = 0;

int userland_launch_shell_once(void) {
    if (shell_spawned) {
        return 0;
    }

    if (userland_spawn_and_schedule("shell", shell_user_main, 5) != 0) {
        klog_printf(KLOG_INFO, "USERLAND: Shell failed to start after roulette win\n");
        return -1;
    }

    shell_spawned = 1;
    return 0;
}

/* Fate hook: start the shell when the wheel of fate declares a win. */
static void userland_fate_hook(const struct fate_result *res) {
    if (!res || !res->is_win) {
        return;
    }

    if (userland_launch_shell_once() != 0) {
        klog_printf(KLOG_INFO, "USERLAND: Shell bootstrap hook failed\n");
    }
}

/* Boot steps: register fate hook then create roulette gatekeeper task. */
static int boot_step_userland_hook(void) {
    fate_register_outcome_hook(userland_fate_hook);
    return 0;
}

static int boot_step_roulette_task(void) {
    return userland_spawn_and_schedule("roulette", roulette_user_main, 5);
}

BOOT_INIT_STEP_WITH_FLAGS(services, "userland fate hook", boot_step_userland_hook, BOOT_INIT_PRIORITY(35));
BOOT_INIT_STEP_WITH_FLAGS(services, "roulette task", boot_step_roulette_task, BOOT_INIT_PRIORITY(40));

