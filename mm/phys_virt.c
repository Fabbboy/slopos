/*
 * SlopOS Memory Management - Physical <-> Virtual translation helpers
 */

#include <stdint.h>
#include <stddef.h>

#include "../drivers/serial.h"
#include "../boot/limine_protocol.h"
#include "memory_layout.h"
#include "phys_virt.h"

static uint64_t cached_identity_limit;
static uint64_t kernel_phys_start;
static uint64_t kernel_phys_end;
static uint64_t kernel_virt_start;
static int translation_initialized;

void *memset(void *dest, int value, size_t n);
extern uint64_t virt_to_phys(uint64_t virt_addr);

void mm_init_phys_virt_helpers(void) {
    const kernel_memory_layout_t *layout = get_kernel_memory_layout();
    if (!layout) {
        cached_identity_limit = 0;
        kernel_phys_start = 0;
        kernel_phys_end = 0;
        kernel_virt_start = 0;
        translation_initialized = 0;
        return;
    }

    kernel_phys_start = layout->kernel_start_phys;
    kernel_phys_end = layout->kernel_end_phys;
    kernel_virt_start = layout->kernel_start_virt;
    cached_identity_limit = layout->identity_map_end;

    translation_initialized = (kernel_phys_end > kernel_phys_start);
}

uint64_t mm_phys_to_virt(uint64_t phys_addr) {
    if (phys_addr == 0) {
        return 0;
    }

    if (is_hhdm_available()) {
        return phys_addr + get_hhdm_offset();
    }

    if (translation_initialized) {
        if (phys_addr >= kernel_phys_start && phys_addr < kernel_phys_end) {
            return kernel_virt_start + (phys_addr - kernel_phys_start);
        }

        if (phys_addr < cached_identity_limit) {
            return phys_addr;
        }
    }

    kprint("mm_phys_to_virt: No mapping available for physical address\n");
    return 0;
}

uint64_t mm_virt_to_phys(uint64_t virt_addr) {
    if (virt_addr == 0) {
        return 0;
    }

    return virt_to_phys(virt_addr);
}

int mm_zero_physical_page(uint64_t phys_addr) {
    if (phys_addr == 0) {
        return -1;
    }

    uint64_t virt = mm_phys_to_virt(phys_addr);
    if (virt == 0) {
        return -1;
    }

    memset((void *)virt, 0, PAGE_SIZE_4KB);
    return 0;
}

