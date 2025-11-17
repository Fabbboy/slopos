/*
 * SlopOS Shutdown Orchestration
 * Provides a reusable shutdown sequence that quiesces the scheduler,
 * terminates process state, and halts hardware in a defined order.
 *
 * The helpers in this file are written to be re-entrant so that panic
 * handlers or future power-management code can safely invoke them even
 * if a shutdown is already in progress.
 */

#include "shutdown.h"
#include "debug.h"
#include "constants.h"
#include "../sched/scheduler.h"
#include "../drivers/serial.h"
#include "../drivers/apic.h"
#include "../mm/phys_virt.h"

#include <stdint.h>
#include <string.h>

/* Track shutdown progress so re-entrant callers can short-circuit safely */
static volatile int shutdown_in_progress = 0;
static volatile int interrupts_quiesced = 0;
static volatile int serial_drained = 0;

/*
 * Disable interrupts, flush pending requests, and mask interrupt sources.
 */
void kernel_quiesce_interrupts(void) {
    __asm__ volatile ("cli");

    if (interrupts_quiesced) {
        return;
    }

    kprintln("Kernel shutdown: quiescing interrupt controllers");

    if (apic_is_available()) {
        apic_send_eoi();
        apic_timer_stop();
        apic_disable();
    }

    interrupts_quiesced = 1;
}

/*
 * Ensure serial buffers are empty so shutdown logs reach the host.
 */
void kernel_drain_serial_output(void) {
    if (serial_drained) {
        return;
    }

    kprintln("Kernel shutdown: draining serial output");

    debug_flush();

    uint16_t kernel_port = serial_get_kernel_output();
    if (kernel_port != COM1_BASE) {
        serial_flush(COM1_BASE);
    }

    serial_drained = 1;
}

/*
 * Execute the full shutdown sequence and halt the CPUs.
 */
void kernel_shutdown(const char *reason) {
    __asm__ volatile ("cli");

    if (shutdown_in_progress) {
        kernel_quiesce_interrupts();
        kernel_drain_serial_output();
        goto halt;
    }

    shutdown_in_progress = 1;

    kprintln("=== Kernel Shutdown Requested ===");
    if (reason) {
        kprint("Reason: ");
        kprintln(reason);
    }

    scheduler_shutdown();

    if (task_shutdown_all() != 0) {
        kprintln("Warning: Failed to terminate one or more tasks");
    }

    task_set_current(NULL);

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();

    kprintln("Kernel shutdown complete. Halting processors.");

halt:
    while (1) {
        __asm__ volatile ("hlt");
    }
}

/*
 * Reboot the system using keyboard controller reset
 * This is the fastest, most reliable reboot method for x86
 */
void kernel_reboot(const char *reason) {
    __asm__ volatile ("cli");

    kprintln("=== Kernel Reboot Requested ===");
    if (reason) {
        kprint("Reason: ");
        kprintln(reason);
    }

    kernel_drain_serial_output();

    kprintln("Rebooting via keyboard controller...");

    // Give serial a moment to flush
    for (volatile int i = 0; i < 1000000; i++) {
        __asm__ volatile ("nop");
    }

    // Keyboard controller reset (port 0x64, command 0xFE)
    // This is the standard x86 reboot mechanism
    __asm__ volatile ("outb %0, %1" : : "a"((uint8_t)0xFE), "Nd"((uint16_t)0x64));

    // If that didn't work, try triple fault (should never get here)
    kprintln("Keyboard reset failed, attempting triple fault...");

    // Load invalid IDT to cause triple fault
    struct {
        uint16_t limit;
        uint64_t base;
    } __attribute__((packed)) invalid_idt = {0, 0};

    __asm__ volatile ("lidt %0" : : "m"(invalid_idt));
    __asm__ volatile ("int $0x03");  // Trigger interrupt with invalid IDT

    // If even that fails, just halt
    while (1) {
        __asm__ volatile ("hlt");
    }
}

/*
 * Execute Kernel: Paint all memory with the sacred 0x69
 *
 * When the kernel falls into the abyss of kernel_panic, this ritual is invoked
 * to cleanse all known memory allocations with the holy value 0x69—a tribute
 * to the "slop" that defined this entire endeavor.
 *
 * The function walks the buddy allocator's page metadata and overwrites every
 * known page with 0x69, creating a visual memorial in memory dumps that shows
 * not empty zeros, but the vibrant evidence of what once was.
 */
void execute_kernel(void) {
    /* Forward declare buddy allocator accessor (we'll need to expose this) */
    extern void buddy_allocator_execute_purification(void);

    kprintln("=== EXECUTING KERNEL PURIFICATION RITUAL ===");
    kprintln("Painting memory with the essence of slop (0x69)...");

    /*
     * Invoke the buddy allocator's execution ritual to walk all pages
     * and memset them with 0x69
     */
    buddy_allocator_execute_purification();

    kprintln("Memory purification complete. The slop has been painted eternal.");
}
