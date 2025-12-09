/*
 * SlopOS Syscall Gateway
 * Defines the minimal ABI for user->kernel transitions via int 0x80.
 */

#ifndef DRIVERS_SYSCALL_H
#define DRIVERS_SYSCALL_H

#include <stdint.h>
#include "../boot/idt.h"
#include "../lib/user_syscall_defs.h"
#include "../lib/syscall_numbers.h"

void syscall_handle(struct interrupt_frame *frame);

#endif /* DRIVERS_SYSCALL_H */

