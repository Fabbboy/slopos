/*
 * SlopOS Memory Management - Central Memory System Initialization
 * Coordinates initialization of all memory management subsystems
 * Provides single entry point for memory system setup during kernel boot
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../lib/klog.h"
#include "../boot/limine_protocol.h"
#include "../drivers/apic.h"
#include "../drivers/serial.h"
#include "../lib/alignment.h"
#include "../third_party/limine/limine.h"
#include "memory_layout.h"
#include "memory_reservations.h"
#include "page_alloc.h"
#include "phys_virt.h"
#include "../boot/kernel_panic.h"

/* Memory subsystem initialization functions */
void init_kernel_memory_layout(void);
int init_kernel_heap(void);
int init_process_vm(void);
void init_paging(void);

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

typedef struct memory_init_state {
    int early_paging_done;
    int memory_layout_done;
    int limine_memmap_parsed;
    int hhdm_received;
    int page_allocator_done;
    int kernel_heap_done;
    int process_vm_done;
    int paging_done;
    uint64_t total_memory_bytes;
    uint64_t available_memory_bytes;
    uint64_t reserved_device_bytes;
    uint32_t memory_regions_count;
    uint32_t reserved_region_count;
    uint64_t hhdm_offset;
    uint32_t tracked_page_frames;
    uint64_t allocator_metadata_bytes;
} memory_init_state_t;

static memory_init_state_t init_state = {0};
static allocator_buffer_plan_t allocator_buffers = {0};
static uint32_t usable_overlap_skips = 0;

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
        mm_reservations_add(kernel_phys, kernel_size,
                            MM_RESERVATION_FIRMWARE_OTHER,
                            MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS |
                            MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT,
                            "Kernel image");
    }

    mm_reservations_add(BOOT_STACK_PHYS_ADDR, BOOT_STACK_SIZE,
                        MM_RESERVATION_FIRMWARE_OTHER,
                        MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                        "Boot stack");

    mm_reservations_add(EARLY_PML4_PHYS_ADDR, PAGE_SIZE_4KB,
                        MM_RESERVATION_FIRMWARE_OTHER,
                        MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                        "Early PML4");

    mm_reservations_add(EARLY_PDPT_PHYS_ADDR, PAGE_SIZE_4KB,
                        MM_RESERVATION_FIRMWARE_OTHER,
                        MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                        "Early PDPT");

    mm_reservations_add(EARLY_PD_PHYS_ADDR, PAGE_SIZE_4KB,
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

    mm_reservations_add(allocator_buffers.reserved_phys_base,
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
                mm_reservations_add(entry->base, entry->length,
                                    MM_RESERVATION_ACPI_RECLAIMABLE,
                                    MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                                    "ACPI reclaimable");
                break;
            case LIMINE_MEMMAP_ACPI_NVS:
                mm_reservations_add(entry->base, entry->length,
                                    MM_RESERVATION_ACPI_NVS,
                                    MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                                    "ACPI NVS");
                break;
            case LIMINE_MEMMAP_FRAMEBUFFER:
                mm_reservations_add(entry->base, entry->length,
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

    mm_reservations_add(phys_base, length,
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

    uint64_t apic_base_msr = read_msr(MSR_APIC_BASE);
    uint64_t apic_phys = apic_base_msr & APIC_BASE_ADDR_MASK;

    if (apic_phys == 0) {
        return;
    }

    mm_reservations_add(apic_phys, 0x1000,
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

    KLOG_BLOCK(KLOG_INFO, {
        kprint("MM: Reserved device regions (");
        kprint_decimal(count);
        kprint(")\n");
        for (uint32_t i = 0; i < count; i++) {
            const mm_reserved_region_t *region = mm_reservations_get(i);
            if (!region) {
                continue;
            }

            const char *label = region->label[0] ? region->label : mm_reservation_type_name(region->type);
            uint64_t region_end = region->phys_base + region->length;

            kprint("  ");
            kprint(label);
            kprint(": 0x");
            kprint_hex(region->phys_base);
            kprint(" - 0x");
            kprint_hex(region_end - 1);
            kprint(" (");
            kprint_decimal((uint32_t)(region->length / 1024));
            kprint(" KB)\n");
        }
        if (total_bytes > 0) {
            kprint("  Total reserved:      ");
            kprint_decimal((uint32_t)(total_bytes / 1024));
            kprint(" KB\n");
        }
    });
}

static void finalize_reserved_regions(void) {
    init_state.reserved_region_count = mm_reservations_count();
    init_state.reserved_device_bytes = mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);

    log_reserved_regions();
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
        KLOG_BLOCK(KLOG_INFO, {
            kprint("MM: Skipping usable subrange overlapping reservation: 0x");
            kprint_hex(aligned_start);
            kprint(" - 0x");
            kprint_hex(aligned_end - 1);
            kprint("\n");
        });
        return;
    }

    init_state.available_memory_bytes += aligned_size;

    if (add_page_alloc_region(aligned_start, aligned_size, EFI_CONVENTIONAL_MEMORY) != 0) {
        KLOG_BLOCK(KLOG_INFO, {
            kprint("MM: WARNING - failed to register page allocator region\n");
        });
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
 * ALLOCATOR BUFFER PREPARATION
 * ======================================================================== */

static uint32_t clamp_required_frames(uint64_t required_frames_64) {
    uint32_t max_supported = page_allocator_max_supported_frames();

    /* If allocator has not been initialized yet, do not cap. */
    if (max_supported == 0) {
        return (uint32_t)required_frames_64;
    }

    if (required_frames_64 > (uint64_t)max_supported) {
        KLOG_BLOCK(KLOG_DEBUG, {
            kprint("MM: WARNING - Limiting tracked page frames to allocator maximum\n");
        });
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

    uint64_t highest_phys_addr = 0;
    const struct limine_memmap_entry *largest_usable = NULL;

    for (uint64_t i = 0; i < memmap->entry_count; i++) {
        const struct limine_memmap_entry *entry = memmap->entries[i];
        if (!entry || entry->length == 0) {
            continue;
        }

        uint64_t entry_end = entry->base + entry->length;
        if (entry_end > highest_phys_addr) {
            highest_phys_addr = entry_end;
        }

        if (entry->type == LIMINE_MEMMAP_USABLE) {
            if (!largest_usable || entry->length > largest_usable->length) {
                largest_usable = entry;
            }
        }
    }

    if (!largest_usable) {
        klog_info("MM: ERROR - No usable memory regions available for allocator metadata");
        return -1;
    }

    if (highest_phys_addr == 0) {
        klog_info("MM: ERROR - Limine reported zero physical memory");
        return -1;
    }

    uint64_t aligned_highest_phys = align_up_u64(highest_phys_addr, PAGE_SIZE_4KB);
    uint64_t required_frames_64 = aligned_highest_phys / PAGE_SIZE_4KB;
    if (required_frames_64 == 0) {
        required_frames_64 = 1;
    }

    uint32_t required_frames = clamp_required_frames(required_frames_64);

    uint64_t max_fit_frames = largest_usable->length / PAGE_SIZE_4KB;
    if (required_frames > max_fit_frames) {
        KLOG_BLOCK(KLOG_DEBUG, {
            kprint("MM: WARNING - Reducing tracked frames to fit largest usable region\n");
        });
        required_frames = (uint32_t)max_fit_frames;
    }

    size_t page_desc_size = page_allocator_descriptor_size();

    uint64_t page_bytes_u64 = (uint64_t)required_frames * (uint64_t)page_desc_size;

    if (page_bytes_u64 == 0) {
        klog_info("MM: ERROR - Calculated zero-sized allocator metadata buffer");
        return -1;
    }

    const size_t descriptor_alignment = 64;
    size_t page_bytes_aligned = (size_t)align_up_u64(page_bytes_u64, descriptor_alignment);
    uint64_t reserved_bytes = align_up_u64((uint64_t)page_bytes_aligned, PAGE_SIZE_4KB);

    uint64_t usable_start = largest_usable->base;
    uint64_t usable_end = largest_usable->base + largest_usable->length;
    uint64_t usable_end_aligned = align_down_u64(usable_end, PAGE_SIZE_4KB);

    if (usable_end_aligned <= usable_start || reserved_bytes > (usable_end_aligned - usable_start)) {
        klog_info("MM: ERROR - Largest usable region too small for allocator metadata");
        return -1;
    }

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

    init_state.allocator_metadata_bytes = page_bytes_u64;

    KLOG_BLOCK(KLOG_DEBUG, {
        kprint("MM: Page allocator metadata reserved at phys 0x");
        kprint_hex(reserve_phys_base);
        kprint(" (");
        kprint_decimal((uint32_t)(reserved_bytes / 1024));
        kprint(" KB)\n");
    });

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
    init_state.early_paging_done = 1;
    return 0;
}

/**
 * Consume Limine memory map and HHDM information
 * Discovers available physical memory regions
 */
static int initialize_memory_discovery(const struct limine_memmap_response *memmap,
                                       uint64_t hhdm_offset) {
    klog_debug("MM: Processing Limine memory map...");

    init_state.total_memory_bytes = 0;
    init_state.available_memory_bytes = 0;
    init_state.memory_regions_count = 0;

    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        klog_info("MM: ERROR - Limine memory map response missing");
        return -1;
    }

    KLOG_BLOCK(KLOG_DEBUG, {
        kprint("MM: Limine memory entries: ");
        kprint_decimal(memmap->entry_count);
        kprint("\n");
    });

    int processed_entries = 0;

    for (uint64_t i = 0; i < memmap->entry_count; i++) {
        const struct limine_memmap_entry *entry = memmap->entries[i];

        if (!entry || entry->length == 0) {
            continue;
        }

        processed_entries++;
        init_state.memory_regions_count++;
        init_state.total_memory_bytes += entry->length;

        if (entry->type == LIMINE_MEMMAP_USABLE) {
            register_usable_region(entry->base, entry->length);
        }
    }

    if (processed_entries == 0) {
        klog_info("MM: ERROR - Limine memory map contained no valid entries");
        return -1;
    }

    init_state.limine_memmap_parsed = 1;
    init_state.hhdm_offset = hhdm_offset;
    init_state.hhdm_received = 1;

    KLOG_BLOCK(KLOG_DEBUG, {
        kprint("MM: HHDM offset: 0x");
        kprint_hex(hhdm_offset);
        kprint("\n");
    });

    if (finalize_page_allocator() != 0) {
        KLOG_BLOCK(KLOG_INFO, {
            kprint("MM: WARNING - page allocator finalization reported issues\n");
        });
    }

    if (usable_overlap_skips == 0) {
        klog_debug("MM: Reserved overlap check passed (no usable subranges skipped)");
    } else {
        KLOG_BLOCK(KLOG_INFO, {
            kprint("MM: Reserved overlap guard skipped ");
            kprint_decimal(usable_overlap_skips);
            kprint(" subrange(s)\n");
        });
    }

    klog_info("MM: Memory discovery completed successfully");
    return 0;
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
    init_state.page_allocator_done = 1;

    init_state.tracked_page_frames = allocator_buffers.page_capacity;

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
    init_state.paging_done = 1;

    /* Initialize kernel heap for dynamic allocation */
    if (init_kernel_heap() != 0) {
        kernel_panic("MM: Kernel heap initialization failed");
        return -1;
    }
    init_state.kernel_heap_done = 1;

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
    init_state.process_vm_done = 1;

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

    kprint("\n========== SlopOS Memory System Initialized ==========\n");
    kprint("Early Paging:          ");
    kprint(init_state.early_paging_done ? "OK" : "FAILED");
    kprint("\n");
    kprint("Memory Layout:         ");
    kprint(init_state.memory_layout_done ? "OK" : "FAILED");
    kprint("\n");
    kprint("Limine Memmap:         ");
    kprint(init_state.limine_memmap_parsed ? "OK" : "FAILED");
    kprint("\n");
    kprint("HHDM Response:         ");
    kprint(init_state.hhdm_received ? "OK" : "MISSING");
    kprint("\n");
    kprint("Page Allocator:        ");
    kprint(init_state.page_allocator_done ? "OK" : "FAILED");
    kprint("\n");
    if (init_state.tracked_page_frames) {
        kprint("Tracked Frames:        ");
        kprint_decimal(init_state.tracked_page_frames);
        kprint("\n");
    }
    if (init_state.allocator_metadata_bytes) {
        kprint("Allocator Metadata:    ");
        kprint_decimal((uint32_t)(init_state.allocator_metadata_bytes / 1024));
        kprint(" KB\n");
    }
    if (init_state.reserved_region_count) {
        kprint("Reserved Regions:      ");
        kprint_decimal(init_state.reserved_region_count);
        kprint("\n");
    }
    if (init_state.reserved_device_bytes) {
        kprint("Reserved Device Mem:   ");
        kprint_decimal((uint32_t)(init_state.reserved_device_bytes / 1024));
        kprint(" KB\n");
    }
    kprint("Kernel Heap:           ");
    kprint(init_state.kernel_heap_done ? "OK" : "FAILED");
    kprint("\n");
    kprint("Process VM:            ");
    kprint(init_state.process_vm_done ? "OK" : "FAILED");
    kprint("\n");
    kprint("Full Paging:           ");
    kprint(init_state.paging_done ? "OK" : "FAILED");
    kprint("\n");

    KLOG_BLOCK(KLOG_DEBUG, {
        if (init_state.total_memory_bytes > 0) {
            kprint("Total Memory:          ");
            kprint_decimal(init_state.total_memory_bytes / (1024 * 1024));
            kprint(" MB\n");
            kprint("Available Memory:      ");
            kprint_decimal(init_state.available_memory_bytes / (1024 * 1024));
            kprint(" MB\n");
        }
        kprint("Memory Regions:        ");
        kprint_decimal(init_state.memory_regions_count);
        kprint(" regions\n");
        kprint("HHDM Offset:           0x");
        kprint_hex(init_state.hhdm_offset);
        kprint("\n");
        kprint("=====================================================\n\n");
    });
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
    KLOG_BLOCK(KLOG_DEBUG, {
        kprint("Limine memmap response at: 0x");
        kprint_hex((uint64_t)(uintptr_t)memmap);
        kprint("\n");
        kprint("Reported HHDM offset: 0x");
        kprint_hex(hhdm_offset);
        kprint("\n");
    });

    if (!memmap || memmap->entry_count == 0 || !memmap->entries) {
        kernel_panic("MM: Missing Limine memory map");
        return -1;
    }

    usable_overlap_skips = 0;

    /* Establish kernel layout before any reservations or helpers */
    init_kernel_memory_layout();
    init_state.memory_layout_done = 1;
    mm_init_phys_virt_helpers();

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

    /* Phase 4: Set up virtual memory management */
    if (initialize_virtual_memory() != 0) {
        return -1;
    }

    /* Phase 5: Set up process memory management */
    if (initialize_process_memory() != 0) {
        return -1;
    }

    /* Display final summary */
    display_memory_summary();

    klog_info("MM: Complete memory system initialization successful!");
    KLOG_BLOCK(KLOG_DEBUG, {
        kprint("MM: Ready for scheduler and video subsystem initialization\n\n");
    });

    return 0;
}

/**
 * Check if memory system is fully initialized
 * @return 1 if fully initialized, 0 otherwise
 */
int is_memory_system_initialized(void) {
    return (init_state.early_paging_done &&
            init_state.memory_layout_done &&
            init_state.limine_memmap_parsed &&
            init_state.hhdm_received &&
            init_state.page_allocator_done &&
            init_state.kernel_heap_done &&
            init_state.process_vm_done &&
            init_state.paging_done);
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
    if (total_memory_out) *total_memory_out = init_state.total_memory_bytes;
    if (available_memory_out) *available_memory_out = init_state.available_memory_bytes;
    if (regions_count_out) *regions_count_out = init_state.memory_regions_count;
}
