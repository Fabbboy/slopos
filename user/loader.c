/*
 * Simple user program loader that delegates to task_create with user mode flags.
 */
#include "loader.h"

uint32_t user_spawn_program(const char *name, task_entry_t entry_point, void *arg, uint8_t priority) {
    if (!entry_point) {
        return INVALID_TASK_ID;
    }
    return task_create(name, entry_point, arg, priority, TASK_FLAG_USER_MODE);
}


