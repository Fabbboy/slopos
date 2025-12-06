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
#include "constants.h"
#include "../sched/scheduler.h"
#include "../drivers/serial.h"
#include "../drivers/apic.h"
#include "../drivers/pit.h"
#include "../mm/page_alloc.h"
#include "../lib/io.h"
#include "../lib/klog.h"
#include "../lib/cpu.h"

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
    cpu_cli();

    if (interrupts_quiesced) {
        return;
    }

    klog(KLOG_INFO, "Kernel shutdown: quiescing interrupt controllers");

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

    klog(KLOG_INFO, "Kernel shutdown: draining serial output");

    serial_flush(SERIAL_COM1_PORT);

    serial_drained = 1;
}

/*
 * Execute the full shutdown sequence and halt the CPUs.
 */
void kernel_shutdown(const char *reason) {
    cpu_cli();

    if (shutdown_in_progress) {
        kernel_quiesce_interrupts();
        kernel_drain_serial_output();
        goto halt;
    }

    shutdown_in_progress = 1;

    klog(KLOG_INFO, "=== Kernel Shutdown Requested ===");
    if (reason) {
        klog_raw(KLOG_INFO, "Reason: ");
        klog(KLOG_INFO, reason);
    }

    scheduler_shutdown();

    if (task_shutdown_all() != 0) {
        klog(KLOG_INFO, "Warning: Failed to terminate one or more tasks");
    }

    task_set_current(NULL);

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();

    klog(KLOG_INFO, "Kernel shutdown complete. Halting processors.");

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
    cpu_cli();

    klog(KLOG_INFO, "=== Kernel Reboot Requested ===");
    if (reason) {
        klog_raw(KLOG_INFO, "Reason: ");
        klog(KLOG_INFO, reason);
    }

    kernel_drain_serial_output();

    klog(KLOG_INFO, "Rebooting via keyboard controller...");

    /* Brief delay to let serial output flush before reset */
    pit_poll_delay_ms(50);

    // Keyboard controller reset (port 0x64, command 0xFE)
    // This is the standard x86 reboot mechanism
    io_outb(0x64, 0xFE);

    // If that didn't work, try triple fault (should never get here)
    klog(KLOG_INFO, "Keyboard reset failed, attempting triple fault...");

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
    klog(KLOG_INFO, "=== EXECUTING KERNEL PURIFICATION RITUAL ===");
    klog(KLOG_INFO, "Painting memory with the essence of slop (0x69)...");

    page_allocator_paint_all(0x69);

    klog(KLOG_INFO, "Memory purification complete. The slop has been painted eternal.");
}
