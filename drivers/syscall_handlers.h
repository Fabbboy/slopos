/*
 * Syscall dispatcher table and per-domain handlers.
 * Keeps syscall.c focused on trap entry and frame plumbing.
 */

#ifndef DRIVERS_SYSCALL_HANDLERS_H
#define DRIVERS_SYSCALL_HANDLERS_H

#include <stdint.h>
#include "syscall.h"
#include "../sched/task.h"

enum syscall_disposition {
    SYSCALL_DISP_OK = 0,
    SYSCALL_DISP_NO_RETURN, /* Handler does not return to the same context */
};

typedef enum syscall_disposition (*syscall_handler_t)(task_t *task, struct interrupt_frame *frame);

struct syscall_entry {
    syscall_handler_t handler;
    const char *name;
};

const struct syscall_entry *syscall_lookup(uint64_t sysno);

#endif /* DRIVERS_SYSCALL_HANDLERS_H */

