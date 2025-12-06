/*
 * SlopOS Memory Management - Physical <-> Virtual translation helpers
 */

#include <stdint.h>
#include <stddef.h>

#include "../drivers/serial.h"
#include "../lib/klog.h"
#include "../boot/limine_protocol.h"
#include "../lib/memory.h"
#include "memory_reservations.h"
#include "paging.h"
#include "phys_virt.h"
#include "../boot/kernel_panic.h"

void mm_init_phys_virt_helpers(void) {
    if (!is_hhdm_available()) {
        kernel_panic("MM: HHDM unavailable; cannot translate physical addresses");
    }
}

uint64_t mm_phys_to_virt(uint64_t phys_addr) {
    if (phys_addr == 0) {
        return 0;
    }

    const mm_reserved_region_t *reservation = mm_reservations_find(phys_addr);
    if (reservation && (reservation->flags & MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT) == 0) {
        klog_printf(KLOG_DEBUG, "mm_phys_to_virt: rejected reserved phys 0x%llx (%s)\n",
                    (unsigned long long)phys_addr,
                    mm_reservation_type_name(reservation->type));
        return 0;
    }

    if (!is_hhdm_available()) {
        klog_printf(KLOG_INFO, "mm_phys_to_virt: HHDM unavailable for 0x%llx\n",
                    (unsigned long long)phys_addr);
        return 0;
    }

    return phys_addr + get_hhdm_offset();
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

void *mm_map_mmio_region(uint64_t phys_addr, size_t size) {
    if (phys_addr == 0 || size == 0) {
        return NULL;
    }

    uint64_t end_addr = phys_addr + (uint64_t)size - 1;
    if (end_addr < phys_addr) {
        klog_printf(KLOG_INFO, "MM: mm_map_mmio_region overflow detected\n");
        return NULL;
    }

    if (!is_hhdm_available()) {
        klog_printf(KLOG_INFO, "MM: mm_map_mmio_region requires HHDM (unavailable)\n");
        return NULL;
    }

    return (void *)(phys_addr + get_hhdm_offset());
}

int mm_unmap_mmio_region(void *virt_addr, size_t size) {
    (void)virt_addr;
    (void)size;
    /* HHDM mappings are static; nothing to do yet. */
    return 0;
}
