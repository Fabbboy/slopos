/*
 * SlopOS Kernel Panic Handler
 * Emergency error handling for critical kernel failures
 * Uses serial driver for reliable output during panic situations
 */

#include <stdint.h>
#include "constants.h"
#include "shutdown.h"
#include "../drivers/serial.h"
#include "../drivers/random.h"

/* Declare execute_kernel for the purification ritual */
extern void execute_kernel(void);

/*
 * Emergency serial output for panic messages
 * Uses emergency serial functions that bypass normal initialization
 */
static void panic_output_char(char c) {
    serial_emergency_putc(c);
}

static void panic_output_string(const char *str) {
    serial_emergency_puts(str);
}

/*
 * Output hexadecimal number for debugging
 */
static void panic_output_hex(uint64_t value) {
    serial_emergency_put_hex(value);
}

/*
 * Output decimal number for debugging
 */
static void panic_output_decimal(uint64_t value) {
    if (value == 0) {
        panic_output_char('0');
        return;
    }

    char buffer[20];
    int pos = 0;

    while (value > 0) {
        buffer[pos++] = '0' + (value % 10);
        value /= 10;
    }

    while (pos > 0) {
        panic_output_char(buffer[--pos]);
    }
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
    __asm__ volatile ("cli");

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
    panic_output_hex(get_current_rip());
    panic_output_string("\n");

    panic_output_string("RSP: ");
    panic_output_hex(get_current_rsp());
    panic_output_string("\n");

    // Output CPU state information
    uint64_t cr0, cr3, cr4;
    __asm__ volatile ("movq %%cr0, %0" : "=r" (cr0));
    __asm__ volatile ("movq %%cr3, %0" : "=r" (cr3));
    __asm__ volatile ("movq %%cr4, %0" : "=r" (cr4));

    panic_output_string("CR0: ");
    panic_output_hex(cr0);
    panic_output_string("\n");

    panic_output_string("CR3: ");
    panic_output_hex(cr3);
    panic_output_string("\n");

    panic_output_string("CR4: ");
    panic_output_hex(cr4);
    panic_output_string("\n");

    panic_output_string("===================\n");
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
    __asm__ volatile ("cli");

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
            // Simple line number output (assuming line < 10000)
            if (line >= 1000) panic_output_char('0' + (line / 1000) % 10);
            if (line >= 100) panic_output_char('0' + (line / 100) % 10);
            if (line >= 10) panic_output_char('0' + (line / 10) % 10);
            panic_output_char('0' + line % 10);
        }
        panic_output_string("\n");
    }

    // Continue with standard panic procedure
    panic_output_string("RIP: ");
    panic_output_hex(get_current_rip());
    panic_output_string("\n");

    panic_output_string("RSP: ");
    panic_output_hex(get_current_rsp());
    panic_output_string("\n");

    panic_output_string("===================\n");
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
 * its own destiny. If the wheel lands on an even number, the kernel enters
 * the abyss of panic. If odd, it survives—at least for now.
 *
 * This is not a call to be taken lightly. It is an embrace of chaos itself,
 * a deliberate surrender to the entropy that flows through Sloptopia.
 *
 * Usage: kernel_roulette() will either cause a graceful panic or return safely,
 * depending entirely on the mercy of random fate.
 */
void kernel_roulette(void) {
    /* Initialize randomness if needed */
    static int roulette_initialized = 0;
    if (!roulette_initialized) {
        random_init();
        roulette_initialized = 1;
    }

    /* Spin the wheel of fate */
    uint32_t fate = random_next();

    /* Display the spin to the world */
    panic_output_string("\n=== KERNEL ROULETTE: Spinning the Wheel of Fate ===\n");
    panic_output_string("Random number: 0x");
    panic_output_hex(fate);
    panic_output_string(" (");
    panic_output_decimal(fate);
    panic_output_string(")\n");

    /* Check if even (bit 0 is 0) or odd (bit 0 is 1) */
    if ((fate & 1) == 0) {
        /* Even: The wheel has decided your fate */
        panic_output_string("Even number. The wheel has spoken. Destiny awaits in the abyss.\n");
        panic_output_string("=== INITIATING KERNEL PANIC ===\n");
        kernel_panic("Kernel roulette has chosen even—fate is sealed");
    } else {
        /* Odd: The kernel survives another day */
        panic_output_string("Odd number. Fortune smiles upon the slop. Kernel survives.\n");
        panic_output_string("=== ROULETTE COMPLETE: NO PANIC TODAY ===\n");
    }
}
