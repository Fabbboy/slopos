#include "memory_reservations.h"

#include <stddef.h>

#include "mm_constants.h"
#include "../lib/klog.h"
#include "../drivers/serial.h"
#include "../lib/alignment.h"
#include "../boot/kernel_panic.h"
#include "../lib/memory.h"
#include "../lib/string.h"
#include "mm_constants.h"

/* Fallback storage if caller does not provide a buffer. */
#define MM_REGION_STATIC_CAP 1024
static mm_region_t static_region_store[MM_REGION_STATIC_CAP];

typedef struct mm_region_store {
    mm_region_t *regions;
    uint32_t capacity;
    uint32_t count;
    uint32_t overflows;
    int configured;
} mm_region_store_t;

static mm_region_store_t region_store = {
    .regions = static_region_store,
    .capacity = MM_REGION_STATIC_CAP,
    .count = 0,
    .overflows = 0,
    .configured = 0,
};

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

static void ensure_storage(void) {
    if (!region_store.regions || region_store.capacity == 0) {
        kernel_panic("MM: region storage not configured");
    }
}

static void clear_region(mm_region_t *region) {
    region->phys_base = 0;
    region->length = 0;
    region->kind = MM_REGION_RESERVED;
    region->type = MM_RESERVATION_ALLOCATOR_METADATA;
    region->flags = 0;
    region->label[0] = '\0';
}

static void clear_store(void) {
    ensure_storage();
    for (uint32_t i = 0; i < region_store.capacity; i++) {
        clear_region(&region_store.regions[i]);
    }
    region_store.count = 0;
    region_store.overflows = 0;
}

void mm_region_map_configure(mm_region_t *buffer, uint32_t capacity) {
    if (!buffer || capacity == 0) {
        kernel_panic("MM: invalid region storage configuration");
    }
    region_store.regions = buffer;
    region_store.capacity = capacity;
    region_store.configured = 1;
    clear_store();
}

void mm_region_map_reset(void) {
    if (!region_store.configured) {
        /* Switch to fallback storage on first reset if user did not configure. */
        region_store.regions = static_region_store;
        region_store.capacity = MM_REGION_STATIC_CAP;
        region_store.configured = 1;
    }
    clear_store();
}

static void insert_slot(uint32_t index) {
    ensure_storage();
    if (region_store.count >= region_store.capacity) {
        region_store.overflows++;
        kernel_panic("MM: region map capacity exceeded");
    }

    if (index > region_store.count) {
        index = region_store.count;
    }

    if (region_store.count > 0 && index < region_store.count) {
        memmove(&region_store.regions[index + 1],
                &region_store.regions[index],
                (region_store.count - index) * sizeof(mm_region_t));
    }
    region_store.count++;
    clear_region(&region_store.regions[index]);
}

static int regions_equivalent(const mm_region_t *a, const mm_region_t *b) {
    if (!a || !b) {
        return 0;
    }
    if (a->kind != b->kind) {
        return 0;
    }
    if (a->kind == MM_REGION_USABLE) {
        return a->flags == b->flags && a->label[0] == b->label[0];
    }
    return a->type == b->type &&
           a->flags == b->flags &&
           strncmp(a->label, b->label, sizeof(a->label)) == 0;
}

static void try_merge_with_neighbors(uint32_t index) {
    if (region_store.count == 0 || index >= region_store.count) {
        return;
    }

    /* Merge with previous if adjacent and equivalent */
    if (index > 0) {
        mm_region_t *curr = &region_store.regions[index];
        mm_region_t *prev = &region_store.regions[index - 1];
        uint64_t prev_end = prev->phys_base + prev->length;
        if (prev_end == curr->phys_base && regions_equivalent(prev, curr)) {
            prev->length += curr->length;
            memmove(curr, curr + 1,
                    (region_store.count - index - 1) * sizeof(mm_region_t));
            region_store.count--;
            if (index > 0) {
                index--;
            }
        }
    }

    /* Merge with next if adjacent and equivalent */
    if (index + 1 < region_store.count) {
        mm_region_t *curr = &region_store.regions[index];
        mm_region_t *next = &region_store.regions[index + 1];
        uint64_t curr_end = curr->phys_base + curr->length;
        if (curr_end == next->phys_base && regions_equivalent(curr, next)) {
            curr->length += next->length;
            memmove(next, next + 1,
                    (region_store.count - index - 2) * sizeof(mm_region_t));
            region_store.count--;
        }
    }
}

static uint32_t find_region_index(uint64_t phys_base) {
    uint32_t idx = 0;
    while (idx < region_store.count &&
           region_store.regions[idx].phys_base + region_store.regions[idx].length <= phys_base) {
        idx++;
    }
    return idx;
}

static void split_region(uint32_t index, uint64_t split_base) {
    if (index >= region_store.count) {
        return;
    }
    mm_region_t *region = &region_store.regions[index];
    uint64_t region_end = region->phys_base + region->length;
    if (split_base <= region->phys_base || split_base >= region_end) {
        return;
    }

    insert_slot(index + 1);
    mm_region_t *right = &region_store.regions[index + 1];
    *right = *region;
    right->phys_base = split_base;
    right->length = region_end - split_base;
    region->length = split_base - region->phys_base;
}

static void overlay_region(uint64_t phys_base, uint64_t length,
                           mm_region_kind_t kind, mm_reservation_type_t type,
                           uint32_t flags, const char *label) {
    if (length == 0) {
        return;
    }

    /* Reject obvious virtual/HHDM addresses that are not physical. */
    if (phys_base >= KERNEL_VIRTUAL_BASE || phys_base >= HHDM_VIRT_BASE) {
        klog_printf(KLOG_INFO, "MM: rejecting virtual overlay base 0x%llx\n",
                    (unsigned long long)phys_base);
        kernel_panic("MM: region overlay received virtual address");
    }

    uint64_t end = phys_base + length;
    if (end <= phys_base) {
        kernel_panic("MM: region overlay overflow");
    }

    uint64_t aligned_base = align_down_u64(phys_base, PAGE_SIZE_4KB);
    uint64_t aligned_end = align_up_u64(end, PAGE_SIZE_4KB);
    if (aligned_end <= aligned_base) {
        kernel_panic("MM: region overlay collapsed");
    }

    uint64_t cursor = aligned_base;
    while (cursor < aligned_end) {
        uint32_t idx = find_region_index(cursor);

        if (idx >= region_store.count || region_store.regions[idx].phys_base > cursor) {
            /* Insert new gap before existing region or append at end. */
            insert_slot(idx);
            region_store.regions[idx].phys_base = cursor;
            region_store.regions[idx].length = aligned_end - cursor;
            region_store.regions[idx].kind = kind;
            region_store.regions[idx].type = type;
            region_store.regions[idx].flags = flags;
            copy_label(region_store.regions[idx].label, label);
            try_merge_with_neighbors(idx);
            break;
        }

        mm_region_t *region = &region_store.regions[idx];
        uint64_t region_end = region->phys_base + region->length;

        /* Split so that [cursor, aligned_end) aligns with region boundaries. */
        split_region(idx, cursor);
        region = &region_store.regions[idx];
        region_end = region->phys_base + region->length;

        uint64_t apply_end = (aligned_end < region_end) ? aligned_end : region_end;
        split_region(idx, apply_end);
        region = &region_store.regions[idx];

        /* Overwrite region slice with new attributes. */
        region->kind = kind;
        region->type = type;
        region->flags = flags;
        copy_label(region->label, label);
        try_merge_with_neighbors(idx);

        cursor = apply_end;
    }
}

int mm_region_add_usable(uint64_t phys_base, uint64_t length, const char *label) {
    if (length == 0) {
        return -1;
    }
    overlay_region(phys_base, length, MM_REGION_USABLE, MM_RESERVATION_FIRMWARE_OTHER, 0, label);
    return 0;
}

int mm_region_reserve(uint64_t phys_base, uint64_t length,
                      mm_reservation_type_t type, uint32_t flags,
                      const char *label) {
    if (length == 0) {
        return -1;
    }
    overlay_region(phys_base, length, MM_REGION_RESERVED, type, flags, label);
    return 0;
}

/* Debug helper: emit all regions with physical ranges. */
void mm_region_dump(enum klog_level level) {
    for (uint32_t i = 0; i < region_store.count; i++) {
        const mm_region_t *region = &region_store.regions[i];
        const char *kind = (region->kind == MM_REGION_USABLE) ? "usable" : "reserved";
        klog_printf(level, "[MM] %s: 0x%llx - 0x%llx (%llu KB) label=%s flags=0x%x\n",
                    kind,
                    (unsigned long long)region->phys_base,
                    (unsigned long long)(region->phys_base + region->length - 1),
                    (unsigned long long)(region->length / 1024),
                    region->label[0] ? region->label : "-",
                    region->flags);
    }
}

uint32_t mm_region_count(void) {
    return region_store.count;
}

const mm_region_t *mm_region_get(uint32_t index) {
    if (index >= region_store.count) {
        return NULL;
    }
    return &region_store.regions[index];
}

uint32_t mm_reservations_count(void) {
    uint32_t count = 0;
    for (uint32_t i = 0; i < region_store.count; i++) {
        if (region_store.regions[i].kind == MM_REGION_RESERVED &&
            region_store.regions[i].length > 0) {
            count++;
        }
    }
    return count;
}

uint32_t mm_reservations_capacity(void) {
    return region_store.capacity;
}

uint32_t mm_reservations_overflow_count(void) {
    return region_store.overflows;
}

const mm_region_t *mm_reservations_get(uint32_t index) {
    /* Iterate deterministic by physical order; skip unusable. */
    uint32_t seen = 0;
    for (uint32_t i = 0; i < region_store.count; i++) {
        const mm_region_t *region = &region_store.regions[i];
        if (region->kind != MM_REGION_RESERVED || region->length == 0) {
            continue;
        }
        if (seen == index) {
            return region;
        }
        seen++;
    }
    return NULL;
}

const mm_region_t *mm_reservations_find(uint64_t phys_addr) {
    for (uint32_t i = 0; i < region_store.count; i++) {
        const mm_region_t *region = &region_store.regions[i];
        if (region->kind != MM_REGION_RESERVED || region->length == 0) {
            continue;
        }
        uint64_t end = region->phys_base + region->length;
        if (phys_addr >= region->phys_base && phys_addr < end) {
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

    for (uint32_t i = 0; i < region_store.count; i++) {
        const mm_region_t *region = &region_store.regions[i];
        if (region->kind != MM_REGION_RESERVED || region->length == 0) {
            continue;
        }
        uint64_t region_end = region->phys_base + region->length;
        if (region->phys_base < end && region_end > phys_base) {
            return 1;
        }
    }

    return 0;
}

void mm_iterate_reserved(mm_region_iter_cb cb, void *ctx) {
    if (!cb) {
        return;
    }

    for (uint32_t i = 0; i < region_store.count; i++) {
        const mm_region_t *region = &region_store.regions[i];
        if (region->kind != MM_REGION_RESERVED || region->length == 0) {
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
    for (uint32_t i = 0; i < region_store.count; i++) {
        const mm_region_t *region = &region_store.regions[i];
        if (region->kind != MM_REGION_RESERVED || region->length == 0) {
            continue;
        }
        if (required_flags != 0 && (region->flags & required_flags) != required_flags) {
            continue;
        }
        total += region->length;
    }
    return total;
}

uint64_t mm_region_total_bytes(mm_region_kind_t kind) {
    uint64_t total = 0;
    for (uint32_t i = 0; i < region_store.count; i++) {
        if (region_store.regions[i].kind == kind) {
            total += region_store.regions[i].length;
        }
    }
    return total;
}

uint64_t mm_region_highest_usable_frame(void) {
    uint64_t highest = 0;
    for (uint32_t i = 0; i < region_store.count; i++) {
        const mm_region_t *region = &region_store.regions[i];
        if (region->kind != MM_REGION_USABLE || region->length == 0) {
            continue;
        }
        uint64_t end = region->phys_base + region->length - 1;
        uint64_t frame = end >> 12;
        if (frame > highest) {
            highest = frame;
        }
    }
    return highest;
}

