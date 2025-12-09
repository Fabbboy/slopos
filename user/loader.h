/*
 * Thin user program loader wrappers.
 */
#ifndef USER_LOADER_H
#define USER_LOADER_H

#include <stdint.h>
#include "../sched/task.h"

uint32_t user_spawn_program(const char *name, task_entry_t entry_point, void *arg, uint8_t priority);

#endif /* USER_LOADER_H */


