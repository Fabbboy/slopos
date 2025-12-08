/*
 * Boot memory bring-up steps.
 * Initializes the memory system and verifies higher-half execution.
 */

#include <stdint.h>
#include "../mm/mm_constants.h"
#include "../mm/memory_init.h"
#include "../lib/klog.h"
#include "../boot/init.h"

static int boot_step_memory_init(void) {
    const struct limine_memmap_response *memmap = boot_get_memmap();
    if (!memmap) {
        klog_info("ERROR: Memory map not available");
        return -1;
    }

    uint64_t hhdm = boot_get_hhdm_offset();

    klog_debug("Initializing memory management from Limine data...");
    if (init_memory_system(memmap, hhdm) != 0) {
        klog_info("ERROR: Memory system initialization failed");
        return -1;
    }
    klog_info("Memory management initialized.");
    return 0;
}

static int boot_step_memory_verify(void) {
    uint64_t stack_ptr;
    __asm__ volatile ("movq %%rsp, %0" : "=r" (stack_ptr));

    if (klog_is_enabled(KLOG_DEBUG)) {
        klog_debug("Stack pointer read successfully!");
        klog_printf(KLOG_INFO, "Current Stack Pointer: 0x%lx\n", stack_ptr);

        void *current_ip = __builtin_return_address(0);
        klog_printf(KLOG_INFO, "Kernel Code Address: 0x%lx\n", (uint64_t)current_ip);

        if ((uint64_t)current_ip >= KERNEL_VIRTUAL_BASE) {
            klog_debug("Running in higher-half virtual memory - CORRECT");
        } else {
            klog_info("WARNING: Not running in higher-half virtual memory");
        }
    }

    return 0;
}

BOOT_INIT_STEP(memory, "memory init", boot_step_memory_init);
BOOT_INIT_STEP(memory, "address verification", boot_step_memory_verify);

