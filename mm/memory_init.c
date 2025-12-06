/*
 * SlopOS Memory Management - Central Memory System Initialization
 * Coordinates initialization of all memory management subsystems
 * Provides single entry point for memory system setup during kernel boot
 */

#include <stdint.h>
#include <stddef.h>
#include <limits.h>
#include "mm_constants.h"
#include "../lib/klog.h"
#include "../boot/limine_protocol.h"
#include "../boot/cpu_defs.h"
#include "../lib/cpu.h"
#include "../drivers/serial.h"
#include "../lib/alignment.h"
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
 * MEMORY INITIALIZATION STATE TRACKING
 * ======================================================================== */

typedef struct allocator_buffer_plan {
    void *page_buffer;
    uint32_t page_capacity;
    size_t page_buffer_bytes;
    uint64_t reserved_phys_base;
    uint64_t reserved_phys_size;
    int prepared;
} allocator_buffer_plan_t;

static allocator_buffer_plan_t allocator_buffers = {0};
static uint32_t usable_overlap_skips = 0;

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
 * INITIALIZATION PHASE TRACKING
 * ======================================================================== */

typedef enum mm_init_phase {
    MM_PHASE_UNINITIALIZED = 0,
    MM_PHASE_LAYOUT_READY,
    MM_PHASE_RESERVATIONS_DONE,
    MM_PHASE_PAGE_ALLOC_READY,
    MM_PHASE_MEMORY_DISCOVERY_DONE,
    MM_PHASE_VIRTUAL_READY,
    MM_PHASE_PROCESS_READY,
    MM_PHASE_COMPLETE
} mm_init_phase_t;

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

static mm_init_phase_t init_phase = MM_PHASE_UNINITIALIZED;
static memory_init_stats_t init_stats = {0};
static int early_paging_ok = 0;

static void add_reservation_or_panic(uint64_t base, uint64_t length,
                                     mm_reservation_type_t type,
                                     uint32_t flags,
                                     const char *label) {
    if (mm_reservations_add(base, length, type, flags, label) != 0) {
        kernel_panic("MM: Failed to record reserved region");
    }
}

/* ========================================================================
 * KERNEL CORE RESERVATIONS
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

/* ========================================================================
 * DEVICE MEMORY RESERVATIONS
 * ======================================================================== */

static void record_allocator_metadata_reservation(void) {
    if (!allocator_buffers.prepared || allocator_buffers.reserved_phys_size == 0) {
        return;
    }

    add_reservation_or_panic(allocator_buffers.reserved_phys_base,
                             allocator_buffers.reserved_phys_size,
                             MM_RESERVATION_ALLOCATOR_METADATA,
                             MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                             "Allocator metadata");
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

/* ========================================================================
 * RESERVATION-AWARE USABLE MEMORY HANDLING
 * ======================================================================== */

static void register_usable_subrange(uint64_t start, uint64_t end) {
    if (end <= start) {
        return;
    }

    uint64_t aligned_start = align_up_u64(start, PAGE_SIZE_4KB);
    uint64_t aligned_end = align_down_u64(end, PAGE_SIZE_4KB);

    if (aligned_end <= aligned_start) {
        return;
    }

    uint64_t aligned_size = aligned_end - aligned_start;

    if (mm_is_range_reserved(aligned_start, aligned_size)) {
        usable_overlap_skips++;
        klog_printf(KLOG_INFO,
                    "MM: Skipping usable subrange overlapping reservation: 0x%llx - 0x%llx\n",
                    (unsigned long long)aligned_start,
                    (unsigned long long)(aligned_end - 1));
        return;
    }

    init_stats.available_memory_bytes += aligned_size;

    if (add_page_alloc_region(aligned_start, aligned_size, EFI_CONVENTIONAL_MEMORY) != 0) {
        klog_printf(KLOG_INFO, "MM: WARNING - failed to register page allocator region\n");
    }
}

static void register_usable_region(uint64_t base, uint64_t length) {
    if (length == 0) {
        return;
    }

    uint64_t end = base + length;
    if (end <= base) {
        return;
    }

    uint64_t cursor = base;
    uint32_t count = mm_reservations_count();

    for (uint32_t i = 0; i < count; i++) {
        const mm_reserved_region_t *reservation = mm_reservations_get(i);
        if (!reservation || reservation->length == 0) {
            continue;
        }

        if ((reservation->flags & MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS) == 0) {
            continue;
        }

        uint64_t res_start = reservation->phys_base;
        uint64_t res_end = reservation->phys_base + reservation->length;

        if (res_start >= end) {
            break;
        }

        if (res_end <= cursor) {
            continue;
        }

        if (res_start > cursor) {
            register_usable_subrange(cursor, res_start);
        }

        if (res_end > cursor) {
            cursor = res_end;
        }

        if (cursor >= end) {
            break;
        }
    }

    if (cursor < end) {
        register_usable_subrange(cursor, end);
    }
}

/* ========================================================================
 * ALLOCATOR COVERAGE CALCULATION
 * ======================================================================== */

static uint64_t highest_usable_frame_index(const struct limine_memmap_response *memmap) {
    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        return 0;
    }

    uint64_t highest_frame = 0;
    uint32_t res_count = mm_reservations_count();

    for (uint64_t i = 0; i < memmap->entry_count; i++) {
        const struct limine_memmap_entry *entry = memmap->entries[i];
        if (!entry || entry->length == 0 || entry->type != LIMINE_MEMMAP_USABLE) {
            continue;
        }

        uint64_t entry_end = entry->base + entry->length;
        if (entry_end <= entry->base) {
            continue;
        }

        uint64_t aligned_start = align_up_u64(entry->base, PAGE_SIZE_4KB);
        uint64_t aligned_end = align_down_u64(entry_end, PAGE_SIZE_4KB);
        if (aligned_end <= aligned_start) {
            continue;
        }

        uint64_t cursor = aligned_start;
        for (uint32_t r = 0; r < res_count; r++) {
            const mm_reserved_region_t *res = mm_reservations_get(r);
            if (!res || res->length == 0) {
                continue;
            }
            if ((res->flags & MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS) == 0) {
                continue;
            }

            uint64_t res_end = res->phys_base + res->length;
            if (res_end <= res->phys_base) {
                continue;
            }

            if (res->phys_base >= aligned_end) {
                break;
            }

            if (res_end <= cursor) {
                continue;
            }

            if (res->phys_base > cursor) {
                uint64_t frame = (res->phys_base - 1) >> 12;
                if (frame > highest_frame) {
                    highest_frame = frame;
                }
            }

            cursor = res_end;
            if (cursor >= aligned_end) {
                break;
            }
        }

        if (cursor < aligned_end) {
            uint64_t frame = (aligned_end - 1) >> 12;
            if (frame > highest_frame) {
                highest_frame = frame;
            }
        }
    }

    return highest_frame;
}

/* ========================================================================
 * ALLOCATOR BUFFER PREPARATION
 * ======================================================================== */

static uint32_t clamp_required_frames(uint64_t required_frames_64) {
    uint32_t max_supported = page_allocator_max_supported_frames();

    /* If allocator has not been initialized yet, do not cap. */
    if (max_supported == 0) {
        return (uint32_t)required_frames_64;
    }

    if (required_frames_64 > (uint64_t)max_supported) {
        klog_printf(KLOG_DEBUG, "MM: WARNING - Limiting tracked page frames to allocator maximum\n");
        return max_supported;
    }
    return (uint32_t)required_frames_64;
}

static int prepare_allocator_buffers(const struct limine_memmap_response *memmap,
                                     uint64_t hhdm_offset) {
    if (allocator_buffers.prepared) {
        return 0;
    }

    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        klog_info("MM: ERROR - Cannot prepare allocator buffers without Limine memmap");
        return -1;
    }

    klog_debug("MM: Planning allocator metadata buffers...");

    uint64_t highest_frame = highest_usable_frame_index(memmap);
    uint64_t required_frames_64 = highest_frame + 1;
    if (required_frames_64 == 0 || required_frames_64 > UINT32_MAX) {
        klog_info("MM: ERROR - Required frame count exceeds supported range");
        return -1;
    }

    uint32_t required_frames = clamp_required_frames(required_frames_64);

    size_t page_desc_size = page_allocator_descriptor_size();

    uint64_t page_bytes_u64 = (uint64_t)required_frames * (uint64_t)page_desc_size;

    if (page_bytes_u64 == 0) {
        klog_info("MM: ERROR - Calculated zero-sized allocator metadata buffer");
        return -1;
    }

    const size_t descriptor_alignment = 64;
    size_t page_bytes_aligned = (size_t)align_up_u64(page_bytes_u64, descriptor_alignment);
    uint64_t reserved_bytes = align_up_u64((uint64_t)page_bytes_aligned, PAGE_SIZE_4KB);

    uint64_t reserve_phys_base = 0;
    for (uint64_t i = 0; i < memmap->entry_count; i++) {
        const struct limine_memmap_entry *entry = memmap->entries[i];
        if (!entry || entry->type != LIMINE_MEMMAP_USABLE || entry->length == 0) {
            continue;
        }

        uint64_t aligned_base = align_up_u64(entry->base, PAGE_SIZE_4KB);
        uint64_t aligned_end = align_down_u64(entry->base + entry->length, PAGE_SIZE_4KB);
        if (aligned_end <= aligned_base || aligned_end - aligned_base < reserved_bytes) {
            continue;
        }

        uint64_t candidate = align_down_u64(aligned_end - reserved_bytes, PAGE_SIZE_4KB);
        if (candidate < aligned_base) {
            candidate = aligned_base;
        }

        if (mm_is_range_reserved(candidate, reserved_bytes)) {
            continue;
        }

        if (candidate >= reserve_phys_base) {
            reserve_phys_base = candidate;
        }
    }

    if (reserve_phys_base == 0) {
        klog_info("MM: ERROR - Failed to find non-overlapping window for allocator metadata");
        return -1;
    }

    uintptr_t reserve_virt_base = (uintptr_t)(reserve_phys_base + hhdm_offset);
    uintptr_t reserve_virt_end = reserve_virt_base + reserved_bytes;

    uintptr_t cursor = align_up_u64(reserve_virt_base, descriptor_alignment);
    uintptr_t page_buffer_virtual = cursor;
    cursor += page_bytes_aligned;
    if (cursor > reserve_virt_end) {
        klog_info("MM: ERROR - Allocator metadata alignment exceeded reserved window");
        return -1;
    }

    allocator_buffers.page_buffer = (void *)page_buffer_virtual;
    allocator_buffers.page_capacity = required_frames;
    allocator_buffers.page_buffer_bytes = page_bytes_u64;
    allocator_buffers.reserved_phys_base = reserve_phys_base;
    allocator_buffers.reserved_phys_size = reserved_bytes;
    allocator_buffers.prepared = 1;

    init_stats.allocator_metadata_bytes = page_bytes_u64;

    klog_printf(KLOG_DEBUG, "MM: Page allocator metadata reserved at phys 0x%llx (%u KB)\n",
                (unsigned long long)reserve_phys_base,
                (uint32_t)(reserved_bytes / 1024));

    return 0;
}

/* ========================================================================
 * MEMORY ARRAYS FOR ALLOCATORS
 * ======================================================================== */

/* Static arrays removed: allocator metadata sized dynamically at runtime */

/* ========================================================================
 * INITIALIZATION SEQUENCE FUNCTIONS
 * ======================================================================== */

/**
 * Initialize early paging structures for kernel boot
 * Must be called first before any other memory operations
 */
static int initialize_early_memory(void) {
    klog_debug("MM: Skipping early paging reinitialization (already configured by bootloader)");
    early_paging_ok = 1;
    return 0;
}

/**
 * Consume Limine memory map and HHDM information
 * Discovers available physical memory regions
 */
static int initialize_memory_discovery(const struct limine_memmap_response *memmap,
                                       uint64_t hhdm_offset) {
    klog_debug("MM: Processing Limine memory map...");

    init_stats.total_memory_bytes = 0;
    init_stats.available_memory_bytes = 0;
    init_stats.memory_regions_count = 0;

    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        klog_info("MM: ERROR - Limine memory map response missing");
        return -1;
    }

    klog_printf(KLOG_DEBUG, "MM: Limine memory entries: %llu\n",
                (unsigned long long)memmap->entry_count);

    int processed_entries = 0;

    for (uint64_t i = 0; i < memmap->entry_count; i++) {
        const struct limine_memmap_entry *entry = memmap->entries[i];

        if (!entry || entry->length == 0) {
            continue;
        }

        processed_entries++;
        init_stats.memory_regions_count++;
        init_stats.total_memory_bytes += entry->length;

        if (entry->type == LIMINE_MEMMAP_USABLE) {
            register_usable_region(entry->base, entry->length);
        }
    }

    if (processed_entries == 0) {
        klog_info("MM: ERROR - Limine memory map contained no valid entries");
        return -1;
    }

    init_phase = MM_PHASE_MEMORY_DISCOVERY_DONE;
    init_stats.hhdm_offset = hhdm_offset;

    klog_printf(KLOG_DEBUG, "MM: HHDM offset: 0x%llx\n",
                (unsigned long long)hhdm_offset);

    if (finalize_page_allocator() != 0) {
        klog_printf(KLOG_INFO, "MM: WARNING - page allocator finalization reported issues\n");
    }

    if (usable_overlap_skips == 0) {
        klog_debug("MM: Reserved overlap check passed (no usable subranges skipped)");
    } else {
        klog_printf(KLOG_INFO, "MM: Reserved overlap guard skipped %u subrange(s)\n",
                    (uint32_t)usable_overlap_skips);
    }

    klog_info("MM: Memory discovery completed successfully");
    return 0;
}

static void validate_allocator_coverage(const struct limine_memmap_response *memmap) {
    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        kernel_panic("MM: Missing memmap for allocator coverage validation");
    }

    if (init_stats.tracked_page_frames == 0) {
        kernel_panic("MM: Allocator coverage validation missing tracked frames");
    }

    uint64_t highest_frame = highest_usable_frame_index(memmap);
    uint64_t needed_frames = highest_frame + 1;

    if (needed_frames > init_stats.tracked_page_frames) {
        klog_printf(KLOG_INFO,
                    "MM: Allocator coverage insufficient (need %llu frames, tracking %u)\n",
                    (unsigned long long)needed_frames,
                    init_stats.tracked_page_frames);
        kernel_panic("MM: Frame descriptor coverage truncated usable memory");
    }

    klog_printf(KLOG_DEBUG,
                "MM: Allocator coverage verified (tracked %u frames, highest frame %llu)\n",
                init_stats.tracked_page_frames,
                (unsigned long long)highest_frame);
}

/**
 * Initialize physical memory allocators
 * Sets up page allocator and buddy allocator with discovered memory
 */
static int initialize_physical_allocators(void) {
    klog_debug("MM: Initializing physical memory allocators...");

    if (!allocator_buffers.prepared) {
        klog_info("MM: ERROR - Allocator buffers not prepared before initialization");
        return -1;
    }

    /* Initialize page allocator with static frame array */
    if (init_page_allocator(allocator_buffers.page_buffer,
                            allocator_buffers.page_capacity) != 0) {
        kernel_panic("MM: Page allocator initialization failed");
        return -1;
    }
    init_phase = MM_PHASE_PAGE_ALLOC_READY;

    init_stats.tracked_page_frames = allocator_buffers.page_capacity;

    klog_debug("MM: Physical memory allocator initialized successfully");
    return 0;
}

/**
 * Initialize kernel memory layout and virtual memory
 * Sets up higher-half mapping and kernel heap
 */
static int initialize_virtual_memory(void) {
    klog_debug("MM: Initializing virtual memory management...");

    /* Initialize full paging system */
    init_paging();

    /* Initialize kernel heap for dynamic allocation */
    if (init_kernel_heap() != 0) {
        kernel_panic("MM: Kernel heap initialization failed");
        return -1;
    }
    init_phase = MM_PHASE_VIRTUAL_READY;

    klog_debug("MM: Virtual memory management initialized successfully");
    return 0;
}

/**
 * Initialize process memory management
 * Sets up per-process virtual memory and region management
 */
static int initialize_process_memory(void) {
    klog_debug("MM: Initializing process memory management...");

    /* Initialize process virtual memory management */
    if (init_process_vm() != 0) {
        kernel_panic("MM: Process VM initialization failed");
        return -1;
    }
    init_phase = MM_PHASE_PROCESS_READY;

    klog_debug("MM: Process memory management initialized successfully");
    return 0;
}

/**
 * Display memory initialization summary
 */
static void display_memory_summary(void) {
    if (!klog_is_enabled(KLOG_DEBUG)) {
        return;
    }

    const int layout_ok = init_phase >= MM_PHASE_LAYOUT_READY;
    const int reservations_ok = init_phase >= MM_PHASE_RESERVATIONS_DONE;
    const int page_alloc_ok = init_phase >= MM_PHASE_PAGE_ALLOC_READY;
    const int discovery_ok = init_phase >= MM_PHASE_MEMORY_DISCOVERY_DONE;
    const int virtual_ok = init_phase >= MM_PHASE_VIRTUAL_READY;
    const int process_ok = init_phase >= MM_PHASE_PROCESS_READY;

    klog_printf(KLOG_INFO, "\n========== SlopOS Memory System Initialized ==========\n");
    klog_printf(KLOG_INFO, "Early Paging:          %s\n", early_paging_ok ? "OK" : "FAILED");
    klog_printf(KLOG_INFO, "Memory Layout:         %s\n", layout_ok ? "OK" : "FAILED");
    klog_printf(KLOG_INFO, "Reservations:          %s\n", reservations_ok ? "OK" : "PENDING");
    klog_printf(KLOG_INFO, "Limine Memmap:         %s\n", discovery_ok ? "OK" : "FAILED");
    klog_printf(KLOG_INFO, "HHDM Response:         %s\n", (init_stats.hhdm_offset != 0) ? "OK" : "MISSING");
    klog_printf(KLOG_INFO, "Page Allocator:        %s\n", page_alloc_ok ? "OK" : "FAILED");
    if (init_stats.tracked_page_frames) {
        klog_printf(KLOG_INFO, "Tracked Frames:        %u\n", init_stats.tracked_page_frames);
    }
    if (init_stats.allocator_metadata_bytes) {
        klog_printf(KLOG_INFO, "Allocator Metadata:    %u KB\n",
                    (uint32_t)(init_stats.allocator_metadata_bytes / 1024));
    }
    if (init_stats.reserved_region_count) {
        klog_printf(KLOG_INFO, "Reserved Regions:      %u\n", init_stats.reserved_region_count);
    }
    if (init_stats.reserved_device_bytes) {
        klog_printf(KLOG_INFO, "Reserved Device Mem:   %u KB\n",
                    (uint32_t)(init_stats.reserved_device_bytes / 1024));
    }
    klog_printf(KLOG_INFO, "Kernel Heap:           %s\n", virtual_ok ? "OK" : "FAILED");
    klog_printf(KLOG_INFO, "Process VM:            %s\n", process_ok ? "OK" : "FAILED");
    klog_printf(KLOG_INFO, "Full Paging:           %s\n", virtual_ok ? "OK" : "FAILED");

    if (init_stats.total_memory_bytes > 0) {
        klog_printf(KLOG_INFO, "Total Memory:          %llu MB\n",
                    (unsigned long long)(init_stats.total_memory_bytes / (1024 * 1024)));
        klog_printf(KLOG_INFO, "Available Memory:      %llu MB\n",
                    (unsigned long long)(init_stats.available_memory_bytes / (1024 * 1024)));
    }
    klog_printf(KLOG_INFO, "Memory Regions:        %u regions\n", init_stats.memory_regions_count);
    klog_printf(KLOG_INFO, "HHDM Offset:           0x%llx\n",
                (unsigned long long)init_stats.hhdm_offset);
    klog_printf(KLOG_INFO, "=====================================================\n\n");
}

/* ========================================================================
 * PUBLIC INTERFACE
 * ======================================================================== */

/**
 * Initialize the complete memory management system
 * Must be called early during kernel boot after basic CPU setup
 *
 * @param memmap Limine memory map response provided by bootloader
 * @param hhdm_offset Higher-half direct mapping offset from Limine
 * @return 0 on success, -1 on failure (calls kernel_panic)
 */
int init_memory_system(const struct limine_memmap_response *memmap,
                       uint64_t hhdm_offset) {
    klog_debug("========== SlopOS Memory System Initialization ==========");
    klog_debug("Initializing complete memory management system...");
    klog_printf(KLOG_DEBUG, "Limine memmap response at: 0x%llx\n",
                (unsigned long long)(uintptr_t)memmap);
    klog_printf(KLOG_DEBUG, "Reported HHDM offset: 0x%llx\n",
                (unsigned long long)hhdm_offset);

    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        kernel_panic("MM: Missing Limine memory map");
        return -1;
    }

    usable_overlap_skips = 0;

    /* Establish kernel layout before any reservations or helpers */
    init_kernel_memory_layout();
    mm_init_phys_virt_helpers();
    init_phase = MM_PHASE_LAYOUT_READY;

    mm_reservations_reset();
    record_kernel_core_reservations();
    record_memmap_reservations(memmap);
    record_framebuffer_reservation();
    record_apic_reservation();

    if (prepare_allocator_buffers(memmap, hhdm_offset) != 0) {
        kernel_panic("MM: Failed to size allocator metadata buffers");
        return -1;
    }

    record_allocator_metadata_reservation();
    finalize_reserved_regions();
    init_phase = MM_PHASE_RESERVATIONS_DONE;

    /* Phase 1: Early paging for basic memory access */
    if (initialize_early_memory() != 0) {
        return -1;
    }

    /* Phase 2: Set up physical memory allocators */
    if (initialize_physical_allocators() != 0) {
        return -1;
    }

    /* Phase 3: Discover available physical memory */
    if (initialize_memory_discovery(memmap, hhdm_offset) != 0) {
        return -1;
    }

    validate_allocator_coverage(memmap);

    /* Phase 4: Set up virtual memory management */
    if (initialize_virtual_memory() != 0) {
        return -1;
    }

    /* Phase 5: Set up process memory management */
    if (initialize_process_memory() != 0) {
        return -1;
    }

    init_phase = MM_PHASE_COMPLETE;

    /* Display final summary */
    display_memory_summary();

    klog_info("MM: Complete memory system initialization successful!");
    klog_printf(KLOG_DEBUG, "MM: Ready for scheduler and video subsystem initialization\n\n");

    return 0;
}

/**
 * Check if memory system is fully initialized
 * @return 1 if fully initialized, 0 otherwise
 */
int is_memory_system_initialized(void) {
    return (init_phase == MM_PHASE_COMPLETE);
}

/**
 * Get memory system statistics
 * @param total_memory_out Output parameter for total system memory
 * @param available_memory_out Output parameter for available memory
 * @param regions_count_out Output parameter for number of memory regions
 */
void get_memory_statistics(uint64_t *total_memory_out,
                          uint64_t *available_memory_out,
                          uint32_t *regions_count_out) {
    if (total_memory_out) *total_memory_out = init_stats.total_memory_bytes;
    if (available_memory_out) *available_memory_out = init_stats.available_memory_bytes;
    if (regions_count_out) *regions_count_out = init_stats.memory_regions_count;
}
