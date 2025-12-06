/*
 * SlopOS Memory Management - Central Memory System Initialization
 * Coordinates initialization of all memory management subsystems
 * Provides single entry point for memory system setup during kernel boot
 */

#include <stdint.h>
#include <stddef.h>
#include <limits.h>
#include <string.h>
#include "mm_constants.h"
#include "../lib/klog.h"
#include "../boot/limine_protocol.h"
#include "../boot/cpu_defs.h"
#include "../lib/cpu.h"
#include "../drivers/serial.h"
#include "../lib/alignment.h"
#include "../lib/memory.h"
#include "../third_party/limine/limine.h"
#include "memory_init.h"
#include "memory_layout.h"
#include "memory_reservations.h"
#include "page_alloc.h"
#include "phys_virt.h"
#include "kernel_heap.h"
#include "process_vm.h"
#include "../boot/kernel_panic.h"

/* ========================================================================
 * KERNEL MEMORY LAYOUT
 * ======================================================================== */

static kernel_memory_layout_t kernel_layout = {0};
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

/* ========================================================================
 * CANONICAL MAP AND STATS
 * ======================================================================== */

typedef enum canonical_region_type {
    CANONICAL_USABLE = 0,
    CANONICAL_RESERVED = 1,
} canonical_region_type_t;

typedef struct canonical_region {
    uint64_t base;
    uint64_t length;
    canonical_region_type_t type;
    uint32_t flags;
    char label[32];
} canonical_region_t;

typedef struct memory_init_stats {
    uint64_t total_memory_bytes;
    uint64_t available_memory_bytes;
    uint64_t reserved_device_bytes;
    uint32_t memory_regions_count;
    uint32_t reserved_region_count;
    uint64_t hhdm_offset;
    uint32_t tracked_page_frames;
    uint64_t allocator_metadata_bytes;
} memory_init_stats_t;

#define MAX_CANONICAL_REGIONS   512
#define MAX_RESERVATION_VIEWS   256
#define DESC_ALIGN_BYTES        64

static canonical_region_t canonical_map[MAX_CANONICAL_REGIONS];
static uint32_t canonical_count = 0;
static memory_init_stats_t init_stats = {0};
static int early_paging_ok = 0;
static int memory_system_initialized = 0;

/* ========================================================================
 * LAYOUT HELPERS
 * ======================================================================== */

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

const kernel_memory_layout_t *get_kernel_memory_layout(void) {
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

/* ========================================================================
 * UTILITIES
 * ======================================================================== */

static void add_reservation_or_panic(uint64_t base, uint64_t length,
                                     mm_reservation_type_t type,
                                     uint32_t flags,
                                     const char *label) {
    if (mm_reservations_add(base, length, type, flags, label) != 0) {
        kernel_panic("MM: Failed to record reserved region");
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

static void canonical_reset(void) {
    for (uint32_t i = 0; i < MAX_CANONICAL_REGIONS; i++) {
        canonical_map[i].base = 0;
        canonical_map[i].length = 0;
        canonical_map[i].type = CANONICAL_RESERVED;
        canonical_map[i].flags = 0;
        canonical_map[i].label[0] = '\0';
    }
    canonical_count = 0;
}

static void canonical_add(uint64_t base, uint64_t length,
                          canonical_region_type_t type,
                          uint32_t flags,
                          const char *label) {
    if (length == 0 || canonical_count >= MAX_CANONICAL_REGIONS) {
        return;
    }
    canonical_region_t *slot = &canonical_map[canonical_count++];
    slot->base = base;
    slot->length = length;
    slot->type = type;
    slot->flags = flags;
    copy_label(slot->label, label);
}

static void canonical_sort(void) {
    for (uint32_t i = 0; i < canonical_count; i++) {
        for (uint32_t j = i + 1; j < canonical_count; j++) {
            if (canonical_map[j].base < canonical_map[i].base) {
                canonical_region_t tmp = canonical_map[i];
                canonical_map[i] = canonical_map[j];
                canonical_map[j] = tmp;
            }
        }
    }
}

static void canonical_sort_and_merge(void) {
    if (canonical_count == 0) {
        return;
    }

    canonical_sort();

    uint32_t write = 0;
    for (uint32_t read = 0; read < canonical_count; read++) {
        canonical_region_t *curr = &canonical_map[read];
        if (curr->length == 0) {
            continue;
        }

        if (write == 0) {
            canonical_map[write++] = *curr;
            continue;
        }

        canonical_region_t *prev = &canonical_map[write - 1];
        uint64_t prev_end = prev->base + prev->length;
        if (prev->type == curr->type &&
            prev->flags == curr->flags &&
            prev_end == curr->base) {
            prev->length += curr->length;
        } else {
            canonical_map[write++] = *curr;
        }
    }
    canonical_count = write;
}

/* ========================================================================
 * RESERVATIONS
 * ======================================================================== */

static void record_kernel_core_reservations(void) {
    const kernel_memory_layout_t *layout = get_kernel_memory_layout();

    if (!layout) {
        klog_info("MM: kernel layout unavailable; cannot reserve kernel image");
        return;
    }

    uint64_t kernel_phys = layout->kernel_start_phys;
    uint64_t kernel_size = (layout->kernel_end_phys > layout->kernel_start_phys) ?
        (layout->kernel_end_phys - layout->kernel_start_phys) : 0;

    if (kernel_size > 0) {
        add_reservation_or_panic(kernel_phys, kernel_size,
                                 MM_RESERVATION_FIRMWARE_OTHER,
                                 MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS |
                                 MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT,
                                 "Kernel image");
    }

    add_reservation_or_panic(BOOT_STACK_PHYS_ADDR, BOOT_STACK_SIZE,
                             MM_RESERVATION_FIRMWARE_OTHER,
                             MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                             "Boot stack");

    add_reservation_or_panic(EARLY_PML4_PHYS_ADDR, PAGE_SIZE_4KB,
                             MM_RESERVATION_FIRMWARE_OTHER,
                             MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                             "Early PML4");

    add_reservation_or_panic(EARLY_PDPT_PHYS_ADDR, PAGE_SIZE_4KB,
                             MM_RESERVATION_FIRMWARE_OTHER,
                             MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                             "Early PDPT");

    add_reservation_or_panic(EARLY_PD_PHYS_ADDR, PAGE_SIZE_4KB,
                             MM_RESERVATION_FIRMWARE_OTHER,
                             MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                             "Early PD");
}

static void record_memmap_reservations(const struct limine_memmap_response *memmap) {
    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        return;
    }

    for (uint64_t i = 0; i < memmap->entry_count; i++) {
        const struct limine_memmap_entry *entry = memmap->entries[i];
        if (!entry || entry->length == 0) {
            continue;
        }

        switch (entry->type) {
            case LIMINE_MEMMAP_ACPI_RECLAIMABLE:
                add_reservation_or_panic(entry->base, entry->length,
                                         MM_RESERVATION_ACPI_RECLAIMABLE,
                                         MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                                         "ACPI reclaimable");
                break;
            case LIMINE_MEMMAP_ACPI_NVS:
                add_reservation_or_panic(entry->base, entry->length,
                                         MM_RESERVATION_ACPI_NVS,
                                         MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                                         "ACPI NVS");
                break;
            case LIMINE_MEMMAP_FRAMEBUFFER:
                add_reservation_or_panic(entry->base, entry->length,
                                         MM_RESERVATION_FRAMEBUFFER,
                                         MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS |
                                         MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT |
                                         MM_RESERVATION_FLAG_MMIO,
                                         "Framebuffer");
                break;
            default:
                break;
        }
    }
}

static void record_framebuffer_reservation(void) {
    uint64_t fb_addr = 0;
    uint32_t width = 0;
    uint32_t height = 0;
    uint32_t pitch = 0;
    uint8_t bpp = 0;

    if (!get_framebuffer_info(&fb_addr, &width, &height, &pitch, &bpp)) {
        return;
    }

    uint64_t phys_base = fb_addr;
    if (is_hhdm_available()) {
        uint64_t hhdm_offset = get_hhdm_offset();
        if (phys_base >= hhdm_offset) {
            phys_base -= hhdm_offset;
        }
    }

    if (phys_base == 0 || pitch == 0 || height == 0) {
        return;
    }

    uint64_t length = (uint64_t)pitch * (uint64_t)height;
    if (length == 0) {
        return;
    }

    add_reservation_or_panic(phys_base, length,
                             MM_RESERVATION_FRAMEBUFFER,
                             MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS |
                             MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT |
                             MM_RESERVATION_FLAG_MMIO,
                             "Framebuffer");
}

static void record_apic_reservation(void) {
    uint32_t eax = 0;
    uint32_t ebx = 0;
    uint32_t ecx = 0;
    uint32_t edx = 0;

    cpuid(1, &eax, &ebx, &ecx, &edx);
    if ((edx & CPUID_FEAT_EDX_APIC) == 0) {
        return;
    }

    uint64_t apic_base_msr = cpu_read_msr(MSR_APIC_BASE);
    uint64_t apic_phys = apic_base_msr & APIC_BASE_ADDR_MASK;

    if (apic_phys == 0) {
        return;
    }

    add_reservation_or_panic(apic_phys, 0x1000,
                             MM_RESERVATION_APIC,
                             MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS |
                             MM_RESERVATION_FLAG_MMIO,
                             "Local APIC");
}

/* ========================================================================
 * CANONICAL MAP BUILDING
 * ======================================================================== */

typedef struct reservation_view {
    uint64_t base;
    uint64_t length;
    uint32_t flags;
    char label[32];
    mm_reservation_type_t type;
} reservation_view_t;

static void sort_reservations(reservation_view_t *views, uint32_t count) {
    for (uint32_t i = 0; i < count; i++) {
        for (uint32_t j = i + 1; j < count; j++) {
            if (views[j].base < views[i].base) {
                reservation_view_t tmp = views[i];
                views[i] = views[j];
                views[j] = tmp;
            }
        }
    }
}

static void carve_usable_range(uint64_t start, uint64_t end,
                               const reservation_view_t *reservations,
                               uint32_t reservation_count) {
    uint64_t cursor = start;
    for (uint32_t i = 0; i < reservation_count; i++) {
        const reservation_view_t *res = &reservations[i];
        if ((res->flags & MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS) == 0) {
            continue;
        }

        uint64_t res_start = res->base;
        uint64_t res_end = res->base + res->length;
        if (res_start >= end) {
            break;
        }
        if (res_end <= cursor) {
            continue;
        }

        if (res_start > cursor) {
            canonical_add(cursor, res_start - cursor, CANONICAL_USABLE, 0, "usable");
        }

        if (res_end > cursor) {
            cursor = res_end;
        }

        if (cursor >= end) {
            break;
        }
    }

    if (cursor < end) {
        canonical_add(cursor, end - cursor, CANONICAL_USABLE, 0, "usable");
    }
}

static uint64_t highest_usable_frame(void) {
    uint64_t highest = 0;
    for (uint32_t i = 0; i < canonical_count; i++) {
        if (canonical_map[i].type != CANONICAL_USABLE || canonical_map[i].length == 0) {
            continue;
        }
        uint64_t end = canonical_map[i].base + canonical_map[i].length - 1;
        uint64_t frame = end >> 12;
        if (frame > highest) {
            highest = frame;
        }
    }
    return highest;
}

static void build_canonical_map(const struct limine_memmap_response *memmap,
                                uint64_t hhdm_offset) {
    canonical_reset();
    init_stats.available_memory_bytes = 0;
    init_stats.tracked_page_frames = 0;
    init_stats.memory_regions_count = memmap ? (uint32_t)memmap->entry_count : 0;
    init_stats.hhdm_offset = hhdm_offset;

    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        kernel_panic("MM: Missing Limine memmap for canonical build");
    }

    uint32_t res_count = mm_reservations_count();
    if (res_count > MAX_RESERVATION_VIEWS) {
        kernel_panic("MM: Reservation view capacity exceeded");
    }

    reservation_view_t res_copy[MAX_RESERVATION_VIEWS];
    memset(res_copy, 0, sizeof(res_copy));
    for (uint32_t i = 0; i < res_count; i++) {
        const mm_reserved_region_t *r = mm_reservations_get(i);
        if (!r) {
            continue;
        }
        res_copy[i].base = r->phys_base;
        res_copy[i].length = r->length;
        res_copy[i].flags = r->flags;
        res_copy[i].type = r->type;
        copy_label(res_copy[i].label, r->label);
    }
    sort_reservations(res_copy, res_count);

    init_stats.total_memory_bytes = 0;

    for (uint64_t i = 0; i < memmap->entry_count; i++) {
        const struct limine_memmap_entry *entry = memmap->entries[i];
        if (!entry || entry->length == 0) {
            continue;
        }

        init_stats.total_memory_bytes += entry->length;
        if (entry->type != LIMINE_MEMMAP_USABLE) {
            continue;
        }

        uint64_t base = align_up_u64(entry->base, PAGE_SIZE_4KB);
        uint64_t end = align_down_u64(entry->base + entry->length, PAGE_SIZE_4KB);
        if (end <= base) {
            continue;
        }

        carve_usable_range(base, end, res_copy, res_count);
    }

    for (uint32_t i = 0; i < res_count; i++) {
        reservation_view_t *r = &res_copy[i];
        if (r->length == 0) {
            continue;
        }
        canonical_add(r->base, r->length, CANONICAL_RESERVED, r->flags, r->label);
    }

    canonical_sort_and_merge();

    for (uint32_t i = 0; i < canonical_count; i++) {
        if (canonical_map[i].type == CANONICAL_USABLE) {
            init_stats.available_memory_bytes += canonical_map[i].length;
        }
    }

    uint64_t highest_frame = highest_usable_frame();
    if (highest_frame >= UINT32_MAX) {
        kernel_panic("MM: Usable memory exceeds supported frame range");
    }
    init_stats.tracked_page_frames = (uint32_t)(highest_frame + 1);
    init_stats.reserved_device_bytes = mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);
    init_stats.reserved_region_count = mm_reservations_count();
}

/* ========================================================================
 * ALLOCATOR METADATA PLANNING
 * ======================================================================== */

typedef struct allocator_plan {
    void *buffer;
    uint64_t phys_base;
    uint64_t bytes;
    uint32_t capacity_frames;
} allocator_plan_t;

static uint64_t select_allocator_window(uint64_t reserved_bytes) {
    for (int32_t i = (int32_t)canonical_count - 1; i >= 0; i--) {
        canonical_region_t *region = &canonical_map[i];
        if (region->type != CANONICAL_USABLE || region->length < reserved_bytes) {
            continue;
        }

        uint64_t region_end = region->base + region->length;
        uint64_t candidate = align_down_u64(region_end - reserved_bytes, PAGE_SIZE_4KB);
        if (candidate < region->base) {
            candidate = region->base;
        }
        return candidate;
    }
    return 0;
}

static allocator_plan_t plan_allocator_metadata(const struct limine_memmap_response *memmap,
                                                uint64_t hhdm_offset) {
    allocator_plan_t plan = {0};

    if (init_stats.tracked_page_frames == 0) {
        kernel_panic("MM: No tracked frames available for allocator sizing");
    }

    uint64_t desc_bytes = (uint64_t)init_stats.tracked_page_frames *
                          (uint64_t)page_allocator_descriptor_size();
    uint64_t aligned_bytes = align_up_u64(desc_bytes, DESC_ALIGN_BYTES);
    aligned_bytes = align_up_u64(aligned_bytes, PAGE_SIZE_4KB);
    init_stats.allocator_metadata_bytes = desc_bytes;

    uint64_t phys_base = select_allocator_window(aligned_bytes);
    if (phys_base == 0) {
        kernel_panic("MM: Failed to find window for allocator metadata");
    }

    add_reservation_or_panic(phys_base,
                             aligned_bytes,
                             MM_RESERVATION_ALLOCATOR_METADATA,
                             MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS |
                             MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT,
                             "Allocator metadata");

    plan.phys_base = phys_base;
    plan.bytes = aligned_bytes;
    plan.capacity_frames = init_stats.tracked_page_frames;
    plan.buffer = (void *)(phys_base + hhdm_offset);

    /* Rebuild canonical map to reflect the newly reserved metadata window. */
    build_canonical_map(memmap, hhdm_offset);

    return plan;
}

/* ========================================================================
 * LOGGING
 * ======================================================================== */

static void log_reserved_regions(void) {
    uint32_t count = mm_reservations_count();

    if (count == 0) {
        klog_info("MM: No device memory reservations detected");
        return;
    }

    uint64_t total_bytes = mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);

    klog_printf(KLOG_INFO, "MM: Reserved device regions (%u)\n", count);
    for (uint32_t i = 0; i < count; i++) {
        const mm_reserved_region_t *region = mm_reservations_get(i);
        if (!region) {
            continue;
        }

        const char *label = region->label[0] ? region->label : mm_reservation_type_name(region->type);
        uint64_t region_end = region->phys_base + region->length;

        klog_printf(KLOG_INFO, "  %s: 0x%llx - 0x%llx (%u KB)\n",
                    label,
                    (unsigned long long)region->phys_base,
                    (unsigned long long)(region_end - 1),
                    (uint32_t)(region->length / 1024));
    }
    if (total_bytes > 0) {
        klog_printf(KLOG_INFO, "  Total reserved:      %u KB\n",
                    (uint32_t)(total_bytes / 1024));
    }
    if (mm_reservations_overflow_count() > 0) {
        klog_printf(KLOG_INFO, "  Reservation drops:   %u (capacity %u)\n",
                    mm_reservations_overflow_count(),
                    mm_reservations_capacity());
    }
}

static void finalize_reserved_regions(void) {
    init_stats.reserved_region_count = mm_reservations_count();
    init_stats.reserved_device_bytes = mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);

    log_reserved_regions();

    if (mm_reservations_overflow_count() > 0) {
        kernel_panic("MM: Reserved region capacity exceeded");
    }
}

static void display_memory_summary(void) {
    klog_printf(KLOG_INFO, "\n========== SlopOS Memory System Initialized ==========\n");
    klog_printf(KLOG_INFO, "Early Paging:          %s\n", early_paging_ok ? "OK" : "SKIPPED");
    klog_printf(KLOG_INFO, "Reserved Regions:      %u\n", init_stats.reserved_region_count);
    klog_printf(KLOG_INFO, "Tracked Frames:        %u\n", init_stats.tracked_page_frames);
    klog_printf(KLOG_INFO, "Allocator Metadata:    %u KB\n",
                (uint32_t)(init_stats.allocator_metadata_bytes / 1024));
    klog_printf(KLOG_INFO, "Reserved Device Mem:   %u KB\n",
                (uint32_t)(init_stats.reserved_device_bytes / 1024));
    klog_printf(KLOG_INFO, "Total Memory:          %llu MB\n",
                (unsigned long long)(init_stats.total_memory_bytes / (1024ULL * 1024ULL)));
    klog_printf(KLOG_INFO, "Available Memory:      %llu MB\n",
                (unsigned long long)(init_stats.available_memory_bytes / (1024ULL * 1024ULL)));
    klog_printf(KLOG_INFO, "Memory Regions:        %u\n", init_stats.memory_regions_count);
    klog_printf(KLOG_INFO, "HHDM Offset:           0x%llx\n",
                (unsigned long long)init_stats.hhdm_offset);
    klog_printf(KLOG_INFO, "=====================================================\n\n");
}

/* ========================================================================
 * PUBLIC INTERFACE
 * ======================================================================== */

int init_memory_system(const struct limine_memmap_response *memmap,
                       uint64_t hhdm_offset) {
    klog_debug("========== SlopOS Memory System Initialization ==========");
    klog_debug("Initializing complete memory management system...");

    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        kernel_panic("MM: Missing Limine memory map");
    }

    init_kernel_memory_layout();
    mm_init_phys_virt_helpers();

    mm_reservations_reset();
    record_kernel_core_reservations();
    record_memmap_reservations(memmap);
    record_framebuffer_reservation();
    record_apic_reservation();

    build_canonical_map(memmap, hhdm_offset);

    allocator_plan_t allocator_plan = plan_allocator_metadata(memmap, hhdm_offset);

    finalize_reserved_regions();

    /* Early paging is already set up by the loader; mark as acknowledged. */
    early_paging_ok = 1;

    if (init_page_allocator(allocator_plan.buffer, allocator_plan.capacity_frames) != 0) {
        kernel_panic("MM: Page allocator initialization failed");
    }

    for (uint32_t i = 0; i < canonical_count; i++) {
        canonical_region_t *region = &canonical_map[i];
        if (region->type != CANONICAL_USABLE) {
            continue;
        }
        if (add_page_alloc_region(region->base, region->length, EFI_CONVENTIONAL_MEMORY) != 0) {
            klog_printf(KLOG_INFO, "MM: WARNING - failed to register page allocator region\n");
        }
    }

    if (finalize_page_allocator() != 0) {
        klog_printf(KLOG_INFO, "MM: WARNING - page allocator finalization reported issues\n");
    }

    init_paging();

    if (init_kernel_heap() != 0) {
        kernel_panic("MM: Kernel heap initialization failed");
    }

    if (init_process_vm() != 0) {
        kernel_panic("MM: Process VM initialization failed");
    }

    memory_system_initialized = 1;
    display_memory_summary();

    klog_info("MM: Complete memory system initialization successful!");
    klog_printf(KLOG_DEBUG, "MM: Ready for scheduler and video subsystem initialization\n\n");
    return 0;
}

int is_memory_system_initialized(void) {
    return memory_system_initialized;
}

void get_memory_statistics(uint64_t *total_memory_out,
                           uint64_t *available_memory_out,
                           uint32_t *regions_count_out) {
    if (total_memory_out) *total_memory_out = init_stats.total_memory_bytes;
    if (available_memory_out) *available_memory_out = init_stats.available_memory_bytes;
    if (regions_count_out) *regions_count_out = init_stats.memory_regions_count;
}

