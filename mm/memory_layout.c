/*
 * SlopOS Memory Layout Management
 * Provides access to linker-defined kernel boundaries.
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../boot/log.h"
#include "memory_layout.h"

static struct kernel_memory_layout kernel_layout = {0};
static int layout_initialized = 0;

void init_kernel_memory_layout(void) {
    extern char _kernel_start[], _kernel_end[];

    kernel_layout.kernel_start_phys = (uint64_t)_kernel_start;
    kernel_layout.kernel_end_phys = (uint64_t)_kernel_end;

    kernel_layout.kernel_start_virt = KERNEL_VIRTUAL_BASE;
    kernel_layout.kernel_end_virt = KERNEL_VIRTUAL_BASE +
        (kernel_layout.kernel_end_phys - kernel_layout.kernel_start_phys);

    kernel_layout.kernel_heap_start =
        (kernel_layout.kernel_end_phys + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);
    kernel_layout.kernel_heap_end = kernel_layout.kernel_heap_start + (16 * 1024 * 1024);

    kernel_layout.kernel_stack_start = BOOT_STACK_PHYS_ADDR;
    kernel_layout.kernel_stack_end = BOOT_STACK_PHYS_ADDR + BOOT_STACK_SIZE;

    kernel_layout.identity_map_end = PAGE_SIZE_1GB;
    kernel_layout.user_space_start = 0x100000;
    kernel_layout.user_space_end = KERNEL_VIRTUAL_BASE - 1;

    layout_initialized = 1;
    boot_log_debug("SlopOS: Kernel memory layout initialized");
}

const struct kernel_memory_layout *get_kernel_memory_layout(void) {
    return layout_initialized ? &kernel_layout : NULL;
}

uint64_t mm_get_kernel_phys_start(void) {
    return kernel_layout.kernel_start_phys;
}

uint64_t mm_get_kernel_phys_end(void) {
    return kernel_layout.kernel_end_phys;
}

uint64_t mm_get_kernel_virt_start(void) {
    return kernel_layout.kernel_start_virt;
}

uint64_t mm_get_identity_map_limit(void) {
    return kernel_layout.identity_map_end;
}
