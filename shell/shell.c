/*
 * Kernel-side glue: when roulette wins, spawn the userland shell task.
 */

#include "shell.h"
#include "../drivers/fate.h"
#include "../sched/task.h"
#include "../sched/scheduler.h"
#include "../boot/init.h"
#include "../lib/klog.h"
#include "../user/shell_user.h"
#include "../user/loader.h"

int shell_launch_once(void) {
    static int shell_spawned = 0;
    if (shell_spawned) {
        return 0;
    }

    uint32_t shell_task_id = user_spawn_program("shell", shell_user_main, NULL, 5);
    if (shell_task_id == INVALID_TASK_ID) {
        klog_printf(KLOG_INFO, "SHELL: Failed to create shell task\n");
        return -1;
    }

    task_t *shell_task;
    if (task_get_info(shell_task_id, &shell_task) != 0) {
        klog_printf(KLOG_INFO, "SHELL: Failed to fetch shell task info\n");
        return -1;
    }

    if (schedule_task(shell_task) != 0) {
        klog_printf(KLOG_INFO, "SHELL: Failed to schedule shell task\n");
        task_terminate(shell_task_id);
        return -1;
    }

    shell_spawned = 1;
    return 0;
}

static void shell_roulette_outcome_hook(const struct fate_result *res) {
    if (!res || !res->is_win) {
        return;
    }

    if (shell_launch_once() != 0) {
        klog_printf(KLOG_INFO, "SHELL: Failed to start after roulette win\n");
    }
}

void shell_register_roulette_hook(void) {
    fate_register_outcome_hook(shell_roulette_outcome_hook);
}

static int boot_step_shell_hook(void) {
    shell_register_roulette_hook();
    return 0;
}

BOOT_INIT_STEP_WITH_FLAGS(services, "shell hook", boot_step_shell_hook, BOOT_INIT_PRIORITY(35));
