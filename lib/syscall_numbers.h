/*
 * Shared syscall numbers for both kernel and user stubs.
 * Keep this header free of kernel-only dependencies.
 */
#ifndef LIB_SYSCALL_NUMBERS_H
#define LIB_SYSCALL_NUMBERS_H

/* Syscall numbers (rax on entry) */
#define SYSCALL_YIELD 0             /* Yield CPU cooperatively */
#define SYSCALL_EXIT 1              /* Terminate current task */
#define SYSCALL_WRITE 2             /* Write bytes to console */
#define SYSCALL_READ 3              /* Read line from console */
#define SYSCALL_ROULETTE 4          /* Spin roulette, returns fate number */
#define SYSCALL_SLEEP_MS 5          /* Sleep for a bounded number of milliseconds */
#define SYSCALL_FB_INFO 6           /* Fetch framebuffer metadata */
#define SYSCALL_GFX_FILL_RECT 7
#define SYSCALL_GFX_DRAW_LINE 8
#define SYSCALL_GFX_DRAW_CIRCLE 9
#define SYSCALL_GFX_DRAW_CIRCLE_FILLED 10
#define SYSCALL_FONT_DRAW 11
#define SYSCALL_RANDOM_NEXT 12
#define SYSCALL_ROULETTE_RESULT 13
#define SYSCALL_FS_OPEN 14
#define SYSCALL_FS_CLOSE 15
#define SYSCALL_FS_READ 16
#define SYSCALL_FS_WRITE 17
#define SYSCALL_FS_STAT 18
#define SYSCALL_FS_MKDIR 19
#define SYSCALL_FS_UNLINK 20
#define SYSCALL_FS_LIST 21
#define SYSCALL_SYS_INFO 22
#define SYSCALL_HALT 23

#endif /* LIB_SYSCALL_NUMBERS_H */


