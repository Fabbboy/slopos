#ifndef MM_MEMORY_RESERVATIONS_H
#define MM_MEMORY_RESERVATIONS_H

#include <stdint.h>
#include "../lib/klog.h"

typedef enum mm_reservation_type {
    MM_RESERVATION_ALLOCATOR_METADATA = 0,
    MM_RESERVATION_FRAMEBUFFER,
    MM_RESERVATION_ACPI_RECLAIMABLE,
    MM_RESERVATION_ACPI_NVS,
    MM_RESERVATION_APIC,
    MM_RESERVATION_FIRMWARE_OTHER
} mm_reservation_type_t;

typedef enum mm_region_kind {
    MM_REGION_USABLE = 0,
    MM_REGION_RESERVED = 1,
} mm_region_kind_t;

typedef struct mm_region {
    uint64_t phys_base;
    uint64_t length;
    mm_region_kind_t kind;
    mm_reservation_type_t type; /* valid when kind == MM_REGION_RESERVED */
    uint32_t flags;
    char label[32];
} mm_region_t;

#define MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS   (1u << 0)
#define MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT (1u << 1)
#define MM_RESERVATION_FLAG_MMIO                 (1u << 2)

void mm_region_map_reset(void);
void mm_region_map_configure(mm_region_t *buffer, uint32_t capacity);
int mm_region_add_usable(uint64_t phys_base, uint64_t length, const char *label);
int mm_region_reserve(uint64_t phys_base, uint64_t length,
                      mm_reservation_type_t type, uint32_t flags,
                      const char *label);

uint32_t mm_region_count(void);
const mm_region_t *mm_region_get(uint32_t index);

uint32_t mm_reservations_count(void);
uint32_t mm_reservations_capacity(void);
uint32_t mm_reservations_overflow_count(void);
const mm_region_t *mm_reservations_get(uint32_t index);
const mm_region_t *mm_reservations_find(uint64_t phys_addr);
int mm_is_reserved(uint64_t phys_addr);
int mm_is_range_reserved(uint64_t phys_base, uint64_t length);
typedef void (*mm_region_iter_cb)(const mm_region_t *region, void *ctx);
void mm_iterate_reserved(mm_region_iter_cb cb, void *ctx);
const char *mm_reservation_type_name(mm_reservation_type_t type);
uint64_t mm_reservations_total_bytes(uint32_t required_flags);
uint64_t mm_region_total_bytes(mm_region_kind_t kind);
uint64_t mm_region_highest_usable_frame(void);
void mm_region_dump(enum klog_level level);

#endif /* MM_MEMORY_RESERVATIONS_H */

