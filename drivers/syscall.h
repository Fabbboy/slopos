/*
 * SlopOS Syscall Gateway
 * Defines the minimal ABI for user->kernel transitions via int 0x80.
 */

#ifndef DRIVERS_SYSCALL_H
#define DRIVERS_SYSCALL_H

#include <stdint.h>
#include "../boot/idt.h"
#include "../lib/user_syscall_defs.h"

/* Syscall numbers (rax on entry) */
#define SYSCALL_YIELD 0   /* Yield CPU cooperatively */
#define SYSCALL_EXIT  1   /* Terminate current task */
#define SYSCALL_WRITE 2   /* Write bytes to console */
#define SYSCALL_READ  3   /* Read line from console */
#define SYSCALL_ROULETTE 4 /* Spin roulette, returns fate number */
#define SYSCALL_SLEEP_MS 5 /* Sleep for a bounded number of milliseconds */
#define SYSCALL_FB_INFO 6  /* Fetch framebuffer metadata */
#define SYSCALL_GFX_FILL_RECT 7
#define SYSCALL_GFX_DRAW_LINE 8
#define SYSCALL_GFX_DRAW_CIRCLE 9
#define SYSCALL_GFX_DRAW_CIRCLE_FILLED 10
#define SYSCALL_FONT_DRAW 11
#define SYSCALL_RANDOM_NEXT 12
#define SYSCALL_ROULETTE_RESULT 13

void syscall_handle(struct interrupt_frame *frame);

#endif /* DRIVERS_SYSCALL_H */

