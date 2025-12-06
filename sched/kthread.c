/*
 * SlopOS Kernel Thread Helpers
 * Thin wrappers around the cooperative scheduler so kernel subsystems
 * can spawn and coordinate internal tasks without touching low-level
 * task management details.
 */

#include "../drivers/serial.h"
#include "kthread.h"
#include "scheduler.h"
#include "../lib/klog.h"

kthread_id_t kthread_spawn(const char *name, task_entry_t entry_point, void *arg) {
    return kthread_spawn_ex(name, entry_point, arg, TASK_PRIORITY_NORMAL, 0);
}

kthread_id_t kthread_spawn_ex(const char *name, task_entry_t entry_point, void *arg,
                              uint8_t priority, uint16_t flags) {
    if (!name || !entry_point) {
        klog_printf(KLOG_INFO, "kthread_spawn_ex: invalid parameters\n");
        return INVALID_TASK_ID;
    }

    uint16_t combined_flags = flags | TASK_FLAG_KERNEL_MODE;
    kthread_id_t id = task_create(name, entry_point, arg, priority, combined_flags);

    if (id == INVALID_TASK_ID) {
        klog_printf(KLOG_INFO, "kthread_spawn_ex: failed to create thread '%s'\n", name);
    }

    return id;
}

void kthread_yield(void) {
    yield();
}

int kthread_join(kthread_id_t thread_id) {
    return task_wait_for(thread_id);
}

void kthread_exit(void) {
    scheduler_task_exit();
    __builtin_unreachable();
}
