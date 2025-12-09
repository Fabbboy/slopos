/*
 * Syscall dispatcher table.
 * Keeps syscall.c focused on trap entry and frame plumbing.
 */

#ifndef DRIVERS_SYSCALL_HANDLERS_H
#define DRIVERS_SYSCALL_HANDLERS_H

#include <stdint.h>
#include "syscall_common.h"

const struct syscall_entry *syscall_lookup(uint64_t sysno);

#endif /* DRIVERS_SYSCALL_HANDLERS_H */

