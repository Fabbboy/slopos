/*
 * SlopOS Kernel Panic Handler Header
 * Function declarations for kernel panic and emergency handling
 */

#ifndef BOOT_KERNEL_PANIC_H
#define BOOT_KERNEL_PANIC_H

/*
 * Main kernel panic routine
 * Displays error information and halts the system
 */
void kernel_panic(const char *message);

/*
 * Kernel panic with additional context information
 */
void kernel_panic_with_context(const char *message, const char *function,
                               const char *file, int line);

/*
 * Assert function for kernel debugging
 */
void kernel_assert(int condition, const char *message);

/*
 * The Wheel of Fate: Kernel Roulette
 * Spins a wheel of random numbers; if even, kernel loses and halts.
 * If odd, kernel survives and continues.
 */
void kernel_roulette(void);

/*
 * Convenience macro for panic with source location
 */
#define KERNEL_PANIC(msg) \
    kernel_panic_with_context(msg, __func__, __FILE__, __LINE__)

#endif /* BOOT_KERNEL_PANIC_H */

