/*
 * SlopOS Kernel Panic Handler
 * Emergency error handling for critical kernel failures
 * Uses serial driver for reliable output during panic situations
 */

#include <stdint.h>
#include "shutdown.h"
#include "../drivers/serial.h"
#include "../drivers/fate.h"
#include "../drivers/wl_currency.h"
#include "../video/roulette.h"
#include "../lib/numfmt.h"
#include "../lib/cpu.h"

static void panic_output_string(const char *str) {
    serial_emergency_puts(str);
}

/*
 * Get current instruction pointer for debugging
 */
static uint64_t get_current_rip(void) {
    uint64_t rip;
    __asm__ volatile ("leaq (%%rip), %0" : "=r" (rip));
    return rip;
}

/*
 * Get current stack pointer for debugging
 */
static uint64_t get_current_rsp(void) {
    uint64_t rsp;
    __asm__ volatile ("movq %%rsp, %0" : "=r" (rsp));
    return rsp;
}

/*
 * Main kernel panic routine
 * Displays error information and halts the system
 */
void kernel_panic(const char *message) {
    // Disable interrupts immediately
    cpu_cli();

    // Output panic header
    panic_output_string("\n\n");
    panic_output_string("=== KERNEL PANIC ===\n");

    // Output panic message
    if (message) {
        panic_output_string("PANIC: ");
        panic_output_string(message);
        panic_output_string("\n");
    } else {
        panic_output_string("PANIC: No message provided\n");
    }

    // Output debugging information
    panic_output_string("RIP: ");
    serial_emergency_put_hex(get_current_rip());
    panic_output_string("\n");

    panic_output_string("RSP: ");
    serial_emergency_put_hex(get_current_rsp());
    panic_output_string("\n");

    // Output CPU state information
    uint64_t cr0, cr3, cr4;
    __asm__ volatile ("movq %%cr0, %0" : "=r" (cr0));
    __asm__ volatile ("movq %%cr3, %0" : "=r" (cr3));
    __asm__ volatile ("movq %%cr4, %0" : "=r" (cr4));

    panic_output_string("CR0: ");
    serial_emergency_put_hex(cr0);
    panic_output_string("\n");

    panic_output_string("CR3: ");
    serial_emergency_put_hex(cr3);
    panic_output_string("\n");

    panic_output_string("CR4: ");
    serial_emergency_put_hex(cr4);
    panic_output_string("\n");

    panic_output_string("===================\n");
    panic_output_string("Skill issue lol\n");
    panic_output_string("System halted.\n");

    /*
     * Invoke the final purification ritual before shutdown
     * Paint all memory with 0x69—the essence of slop itself
     */
    execute_kernel();

    kernel_shutdown(message ? message : "panic");
}

/*
 * Kernel panic with additional context information
 */
void kernel_panic_with_context(const char *message, const char *function,
                              const char *file, int line) {
    // Disable interrupts immediately
    cpu_cli();

    panic_output_string("\n\n");
    panic_output_string("=== KERNEL PANIC ===\n");

    if (message) {
        panic_output_string("PANIC: ");
        panic_output_string(message);
        panic_output_string("\n");
    }

    if (function) {
        panic_output_string("Function: ");
        panic_output_string(function);
        panic_output_string("\n");
    }

    if (file) {
        panic_output_string("File: ");
        panic_output_string(file);
        if (line > 0) {
            panic_output_string(":");
            char line_buf[32];
            if (numfmt_u64_to_decimal((uint64_t)line, line_buf, sizeof(line_buf)) == 0) {
                    serial_emergency_putc('0');
            } else {
                panic_output_string(line_buf);
            }
        }
        panic_output_string("\n");
    }

    // Continue with standard panic procedure
    panic_output_string("RIP: ");
    serial_emergency_put_hex(get_current_rip());
    panic_output_string("\n");

    panic_output_string("RSP: ");
    serial_emergency_put_hex(get_current_rsp());
    panic_output_string("\n");

    panic_output_string("===================\n");
    panic_output_string("Skill issue lol\n");
    panic_output_string("System halted.\n");

    /*
     * Invoke the final purification ritual before shutdown
     * Paint all memory with 0x69—the essence of slop itself
     */
    execute_kernel();

    kernel_shutdown(message ? message : "panic");
}

/*
 * Assert function for kernel debugging
 */
void kernel_assert(int condition, const char *message) {
    if (!condition) {
        kernel_panic(message ? message : "Assertion failed");
    }
}

/*
 * The Wheel of Fate: Kernel Roulette
 *
 * The Scrolls speak of a mystical game inscribed into the very heart of SlopOS:
 * When invoked, the kernel spins a wheel of random numbers, and fate decides
 * its own destiny. If the wheel lands on an even number, the kernel loses
 * and halts forever on the loss screen. If odd, it survives and continues.
 *
 * This is not a call to be taken lightly. It is an embrace of chaos itself,
 * a deliberate surrender to the entropy that flows through Sloptopia.
 *
 * NOW WITH VISUAL GAMBLING ADDICTION!
 *
 * Usage: kernel_roulette() will either halt forever (even) or return safely (odd),
 * depending entirely on the mercy of random fate.
 */
void kernel_roulette(void) {
    struct fate_result res = fate_spin();

    panic_output_string("\n=== KERNEL ROULETTE: Spinning the Wheel of Fate ===\n");
    panic_output_string("Random number: 0x");
    serial_emergency_put_hex(res.value);
    panic_output_string(" (");
    char decimal_buffer[32];
    if (numfmt_u64_to_decimal(res.value, decimal_buffer, sizeof(decimal_buffer)) == 0) {
        decimal_buffer[0] = '0';
        decimal_buffer[1] = '\0';
    }
    serial_emergency_puts(decimal_buffer);
    panic_output_string(")\n");

    if (!res.is_win) {
        panic_output_string("Even number. The wheel has spoken. You have lost.\n");
        panic_output_string("This is INTENTIONAL - keep booting, keep gambling.\n");
        panic_output_string("L bozzo lol\n");
        panic_output_string("=== ROULETTE LOSS: AUTO-REBOOTING TO TRY AGAIN ===\n");
        panic_output_string("The gambling never stops...\n");
    } else {
        panic_output_string("Odd number. Fortune smiles upon the slop. Kernel survives.\n");
        panic_output_string("=== ROULETTE WIN: CONTINUING TO OS ===\n");
    }

    fate_apply_outcome(&res, FATE_RESOLUTION_REBOOT_ON_LOSS);
}
