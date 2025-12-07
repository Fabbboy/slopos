/*
 * SlopOS Syscall Gateway
 * Defines the minimal ABI for user->kernel transitions via int 0x80.
 */

#ifndef DRIVERS_SYSCALL_H
#define DRIVERS_SYSCALL_H

#include <stdint.h>
#include "../boot/idt.h"

/* Syscall numbers (rax on entry) */
#define SYSCALL_YIELD 0   /* Yield CPU cooperatively */
#define SYSCALL_EXIT  1   /* Terminate current task */
#define SYSCALL_WRITE 2   /* Write bytes to console */
#define SYSCALL_READ  3   /* Read line from console */
#define SYSCALL_ROULETTE 4 /* Spin the wheel of fate */

void syscall_handle(struct interrupt_frame *frame);

#endif /* DRIVERS_SYSCALL_H */

