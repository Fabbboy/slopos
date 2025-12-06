/*
 * SlopOS Memory Layout Management
 * Provides access to linker-defined kernel boundaries.
 */

#include <stdint.h>
#include <stddef.h>
#include "mm_constants.h"
#include "../lib/klog.h"
#include "memory_layout.h"

static struct kernel_memory_layout kernel_layout = {0};
static int layout_initialized = 0;
static const process_memory_layout_t process_layout = {
    .code_start = PROCESS_CODE_START_VA,
    .data_start = PROCESS_DATA_START_VA,
    .heap_start = PROCESS_HEAP_START_VA,
    .heap_max = PROCESS_HEAP_MAX_VA,
    .stack_top = PROCESS_STACK_TOP_VA,
    .stack_size = PROCESS_STACK_SIZE_BYTES,
    .user_space_start = USER_SPACE_START_VA,
    .user_space_end = USER_SPACE_END_VA,
};

void init_kernel_memory_layout(void) {
    extern char _kernel_start[], _kernel_end[];

    kernel_layout.kernel_start_phys = (uint64_t)_kernel_start;
    kernel_layout.kernel_end_phys = (uint64_t)_kernel_end;

    kernel_layout.kernel_start_virt = KERNEL_VIRTUAL_BASE;
    kernel_layout.kernel_end_virt = KERNEL_VIRTUAL_BASE +
        (kernel_layout.kernel_end_phys - kernel_layout.kernel_start_phys);

    kernel_layout.kernel_heap_start = KERNEL_HEAP_VBASE;
    kernel_layout.kernel_heap_end = KERNEL_HEAP_VBASE + KERNEL_HEAP_SIZE;

    kernel_layout.kernel_stack_start = BOOT_STACK_PHYS_ADDR;
    kernel_layout.kernel_stack_end = BOOT_STACK_PHYS_ADDR + BOOT_STACK_SIZE;

    kernel_layout.identity_map_end = PAGE_SIZE_1GB;
    kernel_layout.user_space_start = USER_SPACE_START_VA;
    kernel_layout.user_space_end = USER_SPACE_END_VA;

    layout_initialized = 1;
    klog_debug("SlopOS: Kernel memory layout initialized");
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

uint64_t mm_get_kernel_heap_start(void) {
    return kernel_layout.kernel_heap_start;
}

uint64_t mm_get_kernel_heap_end(void) {
    return kernel_layout.kernel_heap_end;
}

uint64_t mm_get_user_space_start(void) {
    return kernel_layout.user_space_start;
}

uint64_t mm_get_user_space_end(void) {
    return kernel_layout.user_space_end;
}

const process_memory_layout_t *mm_get_process_layout(void) {
    return &process_layout;
}
