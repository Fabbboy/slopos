#include "memory_reservations.h"

#include <stddef.h>

#include "mm_constants.h"
#include "../lib/klog.h"
#include "../drivers/serial.h"
#include "../lib/alignment.h"

/*
 * Reservation storage:
 * - Backed by a static pool large enough for typical ACPI + firmware layouts.
 * - We track overflows explicitly so callers can fail hard instead of silently
 *   dropping regions.
 */
#define MM_MAX_RESERVED_REGIONS 256

static mm_reserved_region_t reserved_regions[MM_MAX_RESERVED_REGIONS];
static uint32_t reserved_region_count = 0;
static uint32_t reservation_overflows = 0;

static void clear_region(mm_reserved_region_t *region) {
    region->phys_base = 0;
    region->length = 0;
    region->type = MM_RESERVATION_ALLOCATOR_METADATA;
    region->flags = 0;
    for (size_t i = 0; i < sizeof(region->label); i++) {
        region->label[i] = '\0';
    }
}

static void copy_label(char dest[32], const char *src) {
    if (!src) {
        dest[0] = '\0';
        return;
    }

    size_t i = 0;
    for (; i < 31 && src[i] != '\0'; i++) {
        dest[i] = src[i];
    }
    dest[i] = '\0';
}

void mm_reservations_reset(void) {
    for (uint32_t i = 0; i < MM_MAX_RESERVED_REGIONS; i++) {
        clear_region(&reserved_regions[i]);
    }
    reserved_region_count = 0;
    reservation_overflows = 0;
}

int mm_reservations_add(uint64_t phys_base, uint64_t length,
                        mm_reservation_type_t type, uint32_t flags,
                        const char *label) {
    if (length == 0) {
        klog_info("MM: WARNING - Failed to track reserved region (zero length)");
        return -1;
    }

    uint64_t end = phys_base + length;
    if (end < phys_base) {
        klog_info("MM: WARNING - Reserved region overflow detected");
        return -1;
    }

    uint64_t aligned_base = align_down_u64(phys_base, PAGE_SIZE_4KB);
    uint64_t aligned_end = align_up_u64(end, PAGE_SIZE_4KB);

    if (aligned_end <= aligned_base) {
        klog_info("MM: WARNING - Reserved region collapsed during alignment");
        return -1;
    }

    if (reserved_region_count >= MM_MAX_RESERVED_REGIONS) {
        reservation_overflows++;
        klog_printf(KLOG_INFO,
                    "MM: WARNING - Reserved region capacity exceeded (%u)",
                    MM_MAX_RESERVED_REGIONS);
        return -1;
    }

    uint64_t aligned_length = aligned_end - aligned_base;

    mm_reserved_region_t *slot = &reserved_regions[reserved_region_count];
    slot->phys_base = aligned_base;
    slot->length = aligned_length;
    slot->type = type;
    slot->flags = flags;
    copy_label(slot->label, label);

    reserved_region_count++;
    return 0;
}

uint32_t mm_reservations_count(void) {
    return reserved_region_count;
}

uint32_t mm_reservations_capacity(void) {
    return MM_MAX_RESERVED_REGIONS;
}

uint32_t mm_reservations_overflow_count(void) {
    return reservation_overflows;
}

const mm_reserved_region_t *mm_reservations_get(uint32_t index) {
    if (index >= reserved_region_count) {
        return NULL;
    }
    return &reserved_regions[index];
}

const mm_reserved_region_t *mm_reservations_find(uint64_t phys_addr) {
    for (uint32_t i = 0; i < reserved_region_count; i++) {
        const mm_reserved_region_t *region = &reserved_regions[i];
        if (region->length == 0) {
            continue;
        }

        uint64_t region_end = region->phys_base + region->length;
        if (phys_addr >= region->phys_base && phys_addr < region_end) {
            return region;
        }
    }
    return NULL;
}

int mm_is_reserved(uint64_t phys_addr) {
    return mm_reservations_find(phys_addr) != NULL;
}

int mm_is_range_reserved(uint64_t phys_base, uint64_t length) {
    if (length == 0) {
        return 0;
    }

    uint64_t end = phys_base + length;
    if (end <= phys_base) {
        return 1;
    }

    for (uint32_t i = 0; i < reserved_region_count; i++) {
        const mm_reserved_region_t *region = &reserved_regions[i];
        if (region->length == 0) {
            continue;
        }

        uint64_t region_end = region->phys_base + region->length;
        if (region->phys_base < end && region_end > phys_base) {
            return 1;
        }
    }

    return 0;
}

void mm_iterate_reserved(mm_reservation_iter_cb cb, void *ctx) {
    if (!cb) {
        return;
    }

    for (uint32_t i = 0; i < reserved_region_count; i++) {
        const mm_reserved_region_t *region = &reserved_regions[i];
        if (region->length == 0) {
            continue;
        }
        cb(region, ctx);
    }
}

const char *mm_reservation_type_name(mm_reservation_type_t type) {
    switch (type) {
        case MM_RESERVATION_ALLOCATOR_METADATA:
            return "allocator metadata";
        case MM_RESERVATION_FRAMEBUFFER:
            return "framebuffer";
        case MM_RESERVATION_ACPI_RECLAIMABLE:
            return "acpi reclaim";
        case MM_RESERVATION_ACPI_NVS:
            return "acpi nvs";
        case MM_RESERVATION_APIC:
            return "apic";
        case MM_RESERVATION_FIRMWARE_OTHER:
            return "firmware";
        default:
            return "reserved";
    }
}

uint64_t mm_reservations_total_bytes(uint32_t required_flags) {
    uint64_t total = 0;
    for (uint32_t i = 0; i < reserved_region_count; i++) {
        const mm_reserved_region_t *region = &reserved_regions[i];
        if (region->length == 0) {
            continue;
        }

        if (required_flags != 0 && (region->flags & required_flags) != required_flags) {
            continue;
        }

        total += region->length;
    }
    return total;
}

