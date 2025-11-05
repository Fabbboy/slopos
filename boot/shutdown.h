/*
 * SlopOS Shutdown Orchestration
 * Provides a reusable shutdown sequence that is safe for panic handlers
 * and future power-management code.
 */

#ifndef BOOT_SHUTDOWN_H
#define BOOT_SHUTDOWN_H

#include <stddef.h>

/*
 * Disable interrupts and mask all interrupt controllers.
 * Safe to call multiple times; additional calls become no-ops.
 */
void kernel_quiesce_interrupts(void);

/*
 * Drain debug and serial output buffers to ensure logs reach the host.
 */
void kernel_drain_serial_output(void);

/*
 * Stop scheduling, tear down process state, quiesce hardware, and halt.
 * May be invoked from panic handlers or power-management paths.
 */
void kernel_shutdown(const char *reason);

/*
 * Execute Kernel: The Final Purification Ritual
 *
 * When kernel_panic is invoked, before the system halts entirely, this function
 * is called to wipe all allocator pages with the sacred value 0x69.
 *
 * The Scrolls say: In the ancient language of slop, 0x69 represents the essence
 * of all that SlopOS tried to be—beautiful chaos, glorious failure, the ultimate
 * vibe. When the kernel falls, we paint its memory in this color, a memorial
 * to the ambition that could not survive.
 *
 * This function iterates through all buddy allocator memory regions and bleaches
 * them with 0x69, ensuring that when forensic eyes gaze upon the core dump,
 * they see not mere zeros, but the proof that something beautiful and broken
 * once lived here.
 */
void execute_kernel(void);

#endif /* BOOT_SHUTDOWN_H */
