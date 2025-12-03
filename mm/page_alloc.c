/*
 * SlopOS Memory Management - Physical Page Frame Allocator
 * Manages allocation and deallocation of physical memory pages
 * Serves as the canonical physical allocator for paging, heap, and VM subsystems
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../boot/log.h"
#include "../drivers/serial.h"
#include "page_alloc.h"
#include "phys_virt.h"

/* Forward declarations */
void kernel_panic(const char *message);

/* ========================================================================
 * PHYSICAL PAGE FRAME CONSTANTS
 * ======================================================================== */

/* Physical page frame states */
#define PAGE_FRAME_FREE               0x00   /* Available for allocation */
#define PAGE_FRAME_ALLOCATED          0x01   /* Currently allocated */
#define PAGE_FRAME_RESERVED           0x02   /* Reserved by system */
#define PAGE_FRAME_KERNEL             0x03   /* Kernel-only page */
#define PAGE_FRAME_DMA                0x04   /* DMA-capable page */

/* Maximum physical pages we can track (4GB / 4KB = 1M pages) */
#define MAX_PHYSICAL_PAGES            1048576
#define INVALID_PAGE_FRAME            0xFFFFFFFF
#define DMA_MEMORY_LIMIT              0x01000000ULL

/* Page frame allocation flags */
#define ALLOC_FLAG_ZERO               0x01   /* Zero the page after allocation */
#define ALLOC_FLAG_DMA                0x02   /* Allocate DMA-capable page */
#define ALLOC_FLAG_KERNEL             0x04   /* Kernel-only allocation */

/* ========================================================================
 * PAGE FRAME TRACKING STRUCTURES
 * ======================================================================== */

/* Physical page frame descriptor */
typedef struct page_frame {
    uint32_t ref_count;           /* Reference count for sharing */
    uint8_t state;                /* Page frame state */
    uint8_t flags;                /* Page frame flags */
    uint16_t order;               /* Buddy allocator order (for multi-page blocks) */
    uint32_t next_free;           /* Next free page frame (for free lists) */
} page_frame_t;

/* Physical memory region information */
typedef struct phys_region {
    uint64_t start_addr;          /* Start physical address */
    uint64_t size;                /* Size in bytes */
    uint32_t start_frame;         /* First page frame number */
    uint32_t num_frames;          /* Number of page frames */
    uint8_t type;                 /* Memory type (from EFI) */
    uint8_t available;            /* Available for allocation */
} phys_region_t;

/* Page frame allocator state */
typedef struct page_allocator {
    page_frame_t *frames;         /* Array of page frame descriptors */
    uint32_t total_frames;        /* Total number of page frames */
    uint32_t free_frames;         /* Number of free page frames */
    uint32_t allocated_frames;    /* Number of allocated page frames */
    uint32_t reserved_frames;     /* Number of reserved page frames */
    phys_region_t regions[MAX_MEMORY_REGIONS];  /* Physical memory regions */
    uint32_t num_regions;         /* Number of memory regions */
    uint32_t free_list_head;      /* Head of free page list */
} page_allocator_t;

/* Global page allocator instance */
static page_allocator_t page_allocator = {0};

/* ========================================================================
 * UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Convert physical address to page frame number
 */
static inline uint32_t phys_to_frame(uint64_t phys_addr) {
    return (uint32_t)(phys_addr >> 12);  /* Divide by 4KB */
}

/*
 * Convert page frame number to physical address
 */
static inline uint64_t frame_to_phys(uint32_t frame_num) {
    return (uint64_t)frame_num << 12;  /* Multiply by 4KB */
}

/*
 * Check if a page frame number is valid
 */
static inline int is_valid_frame(uint32_t frame_num) {
    return frame_num < page_allocator.total_frames;
}

/*
 * Get page frame descriptor for frame number
 */
static inline page_frame_t *get_frame_desc(uint32_t frame_num) {
    if (!is_valid_frame(frame_num)) {
        return NULL;
    }
    return &page_allocator.frames[frame_num];
}

/* Forward declarations for helpers used before definition */
static void add_to_free_list(uint32_t frame_num);
#if defined(PAGE_ALLOC_DEBUG)
static void page_alloc_debug_self_test(void);
static uint64_t alloc_page_frames(uint32_t count, uint32_t flags);
static int unlink_frame_from_free_list(uint32_t frame_num);
static void rollback_contiguous_allocation(uint32_t start_frame, uint32_t count);
static int find_contiguous_frames(uint32_t count, uint32_t flags, uint32_t *start_frame_out);
static int frame_satisfies_flags(uint32_t frame_num, uint32_t flags);
#endif

/* ========================================================================
 * DEBUG LOGGING HELPERS
 * ======================================================================== */

#ifdef PAGE_ALLOC_DEBUG
static void page_alloc_log_contiguous(uint64_t phys_addr, uint32_t count) {
    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("alloc_page_frames: allocated ");
        kprint_decimal(count);
        kprint(" pages @ ");
        kprint_hex(phys_addr);
        kprint("\n");
    });
}
#else
#define page_alloc_log_contiguous(phys_addr, count) ((void)0)
#endif

/* ========================================================================
 * INTERNAL HELPERS
 * ======================================================================== */

static uint8_t page_state_for_flags(uint32_t flags) {
    if (flags & ALLOC_FLAG_DMA) {
        return PAGE_FRAME_DMA;
    }
    if (flags & ALLOC_FLAG_KERNEL) {
        return PAGE_FRAME_KERNEL;
    }
    return PAGE_FRAME_ALLOCATED;
}

#if defined(PAGE_ALLOC_DEBUG)
static int frame_satisfies_flags(uint32_t frame_num, uint32_t flags) {
    page_frame_t *frame = get_frame_desc(frame_num);
    if (!frame || frame->state != PAGE_FRAME_FREE) {
        return 0;
    }

    if (flags & ALLOC_FLAG_DMA) {
        uint64_t phys_addr = frame_to_phys(frame_num);
        if (phys_addr >= DMA_MEMORY_LIMIT) {
            return 0;
        }
    }

    return 1;
}
#endif

static int frame_state_is_allocated(uint8_t state) {
    return state == PAGE_FRAME_ALLOCATED ||
           state == PAGE_FRAME_KERNEL ||
           state == PAGE_FRAME_DMA;
}

#if defined(PAGE_ALLOC_DEBUG)
static int unlink_frame_from_free_list(uint32_t frame_num) {
    if (!is_valid_frame(frame_num)) {
        return -1;
    }

    uint32_t current = page_allocator.free_list_head;
    uint32_t previous = INVALID_PAGE_FRAME;

    while (current != INVALID_PAGE_FRAME) {
        if (current == frame_num) {
            page_frame_t *frame = get_frame_desc(frame_num);

            if (previous == INVALID_PAGE_FRAME) {
                page_allocator.free_list_head = frame->next_free;
            } else {
                page_frame_t *prev_frame = get_frame_desc(previous);
                if (prev_frame) {
                    prev_frame->next_free = frame->next_free;
                }
            }

            frame->next_free = INVALID_PAGE_FRAME;
            frame->state = PAGE_FRAME_ALLOCATED;
            frame->ref_count = 0;

            if (page_allocator.free_frames > 0) {
                page_allocator.free_frames--;
            }
            page_allocator.allocated_frames++;

            return 0;
        }

        previous = current;
        page_frame_t *current_frame = get_frame_desc(current);
        if (!current_frame) {
            break;
        }
        current = current_frame->next_free;
    }

    return -1;
}

static void rollback_contiguous_allocation(uint32_t start_frame, uint32_t count) {
    for (uint32_t i = 0; i < count; i++) {
        uint32_t frame_num = start_frame + i;
        if (!is_valid_frame(frame_num)) {
            continue;
        }

        page_frame_t *frame = get_frame_desc(frame_num);
        if (!frame) {
            continue;
        }

        frame->ref_count = 0;
        frame->flags = 0;
        frame->order = 0;

        if (page_allocator.allocated_frames > 0) {
            page_allocator.allocated_frames--;
        }

        add_to_free_list(frame_num);
    }
}

static int find_contiguous_frames(uint32_t count, uint32_t flags, uint32_t *start_frame_out) {
    if (!start_frame_out || count == 0) {
        return -1;
    }

    uint32_t consecutive = 0;
    uint32_t candidate_start = 0;

    for (uint32_t frame_num = 0; frame_num < page_allocator.total_frames; frame_num++) {
        page_frame_t *frame = get_frame_desc(frame_num);
        if (!frame) {
            consecutive = 0;
            continue;
        }

        if (frame->state == PAGE_FRAME_RESERVED) {
            consecutive = 0;
            continue;
        }

        if (frame_state_is_allocated(frame->state)) {
            consecutive = 0;
            continue;
        }

        if (frame_satisfies_flags(frame_num, flags)) {
            if (consecutive == 0) {
                candidate_start = frame_num;
            }

            consecutive++;

            if (consecutive == count) {
                *start_frame_out = candidate_start;
                return 0;
            }
        } else {
            consecutive = 0;
        }
    }

    return -1;
}
#endif /* PAGE_ALLOC_DEBUG */

#ifdef PAGE_ALLOC_DEBUG
static void page_alloc_debug_self_test(void) {
    static const uint32_t sample_sizes[] = {2, 4, 16, 64};
    static const uint32_t sample_count = sizeof(sample_sizes) / sizeof(sample_sizes[0]);

    kprintln("[page_alloc] Running contiguous allocation self-test");

    for (uint32_t i = 0; i < sample_count; i++) {
        uint32_t count = sample_sizes[i];
        uint64_t phys_base = alloc_page_frames(count, ALLOC_FLAG_KERNEL);

        if (phys_base == 0) {
            kprint("[page_alloc] Self-test failed to allocate ");
            kprint_decimal(count);
            kprintln(" pages");
            continue;
        }

        uint32_t first_frame = phys_to_frame(phys_base);
        int contiguous_ok = 1;

        for (uint32_t page = 0; page < count; page++) {
            uint64_t expected = frame_to_phys(first_frame + page);
            uint64_t actual = phys_base + ((uint64_t)page << 12);
            if (actual != expected) {
                contiguous_ok = 0;
                break;
            }
        }

        if (!contiguous_ok) {
            kprint("[page_alloc] Contiguity check failed for ");
            kprint_decimal(count);
            kprintln(" pages");
        }

        for (uint32_t page = 0; page < count; page++) {
            uint64_t phys_page = phys_base + ((uint64_t)page << 12);
            if (free_page_frame(phys_page) != 0) {
                kprint("[page_alloc] Self-test failed to free page index ");
                kprint_decimal(page);
                kprintln("");
            }
        }
    }

    kprintln("[page_alloc] Contiguous allocation self-test complete");
}
#endif


/* ========================================================================
 * FREE LIST MANAGEMENT
 * ======================================================================== */

/*
 * Add page frame to free list
 */
static void add_to_free_list(uint32_t frame_num) {
    if (!is_valid_frame(frame_num)) {
        boot_log_info("add_to_free_list: Invalid frame number");
        return;
    }

    page_frame_t *frame = get_frame_desc(frame_num);
    frame->next_free = page_allocator.free_list_head;
    page_allocator.free_list_head = frame_num;
    frame->state = PAGE_FRAME_FREE;
    frame->flags = 0;
    frame->order = 0;
    frame->ref_count = 0;
    page_allocator.free_frames++;
}

/*
 * Remove page frame from free list
 * Returns frame number, or INVALID_PAGE_FRAME if list is empty
 */
static uint32_t remove_from_free_list(void) {
    if (page_allocator.free_list_head == INVALID_PAGE_FRAME) {
        return INVALID_PAGE_FRAME;
    }

    uint32_t frame_num = page_allocator.free_list_head;
    page_frame_t *frame = get_frame_desc(frame_num);

    page_allocator.free_list_head = frame->next_free;
    frame->next_free = INVALID_PAGE_FRAME;
    frame->state = PAGE_FRAME_ALLOCATED;
    frame->ref_count = 0;
    page_allocator.free_frames--;
    page_allocator.allocated_frames++;

    return frame_num;
}

/* ========================================================================
 * PAGE FRAME ALLOCATION AND DEALLOCATION
 * ======================================================================== */

/*
 * Allocate a single physical page frame
 * Returns physical address of allocated page, 0 on failure
 */
uint64_t alloc_page_frame(uint32_t flags) {
    uint32_t frame_num = remove_from_free_list();

    if (frame_num == INVALID_PAGE_FRAME) {
        boot_log_info("alloc_page_frame: No free pages available");
        return 0;
    }

    page_frame_t *frame = get_frame_desc(frame_num);
    frame->ref_count = 1;
    frame->flags = flags;
    frame->order = 0;  /* Single page */
    frame->state = page_state_for_flags(flags);

    uint64_t phys_addr = frame_to_phys(frame_num);

    /* Zero page if requested */
    if (flags & ALLOC_FLAG_ZERO) {
        if (mm_zero_physical_page(phys_addr) != 0) {
            frame->ref_count = 0;
            frame->flags = 0;
            frame->order = 0;
            add_to_free_list(frame_num);
            if (page_allocator.allocated_frames > 0) {
                page_allocator.allocated_frames--;
            }
            return 0;
        }
    }

    return phys_addr;
}

#if defined(PAGE_ALLOC_DEBUG)
/*
 * Allocate multiple contiguous physical page frames
 * Returns physical address of first page, 0 on failure
 */
static uint64_t alloc_page_frames(uint32_t count, uint32_t flags) {
    if (count == 0) {
        return 0;
    }

    if (count == 1) {
        return alloc_page_frame(flags);
    }

    uint32_t start_frame = 0;
    if (find_contiguous_frames(count, flags, &start_frame) != 0) {
        boot_log_info("alloc_page_frames: Unable to satisfy contiguous allocation");
        return 0;
    }

    uint32_t frames_removed = 0;
    for (uint32_t i = 0; i < count; i++) {
        uint32_t frame_num = start_frame + i;

        if (!frame_satisfies_flags(frame_num, flags)) {
            break;
        }

        if (unlink_frame_from_free_list(frame_num) != 0) {
            break;
        }

        frames_removed++;
    }

    if (frames_removed != count) {
        rollback_contiguous_allocation(start_frame, frames_removed);
        boot_log_info("alloc_page_frames: Failed to unlink frames from free list");
        return 0;
    }

    if (flags & ALLOC_FLAG_ZERO) {
        for (uint32_t i = 0; i < count; i++) {
            uint64_t phys_addr = frame_to_phys(start_frame + i);
            if (mm_zero_physical_page(phys_addr) != 0) {
                rollback_contiguous_allocation(start_frame, count);
                boot_log_info("alloc_page_frames: Zeroing contiguous pages failed");
                return 0;
            }
        }
    }

    for (uint32_t i = 0; i < count; i++) {
        uint32_t frame_num = start_frame + i;
        page_frame_t *frame = get_frame_desc(frame_num);
        if (!frame) {
            continue;
        }

        frame->ref_count = 1;
        frame->flags = flags;
        frame->order = 0;
        frame->state = page_state_for_flags(flags);
    }

    uint64_t start_phys = frame_to_phys(start_frame);
    page_alloc_log_contiguous(start_phys, count);
    return start_phys;
}
#endif /* PAGE_ALLOC_DEBUG */

/*
 * Free a physical page frame
 * Returns 0 on success, -1 on failure
 */
int free_page_frame(uint64_t phys_addr) {
    uint32_t frame_num = phys_to_frame(phys_addr);

    if (!is_valid_frame(frame_num)) {
        boot_log_info("free_page_frame: Invalid physical address");
        return -1;
    }

    page_frame_t *frame = get_frame_desc(frame_num);

    if (!frame_state_is_allocated(frame->state)) {
        boot_log_info("free_page_frame: Page not allocated");
        return -1;
    }

    if (frame->ref_count > 1) {
        /* Decrease reference count but don't free yet */
        frame->ref_count--;
        return 0;
    }

    /* Free the page frame */
    frame->ref_count = 0;
    frame->flags = 0;
    frame->order = 0;

    add_to_free_list(frame_num);
    if (page_allocator.allocated_frames > 0) {
        page_allocator.allocated_frames--;
    }

    return 0;
}

/*
 * Increase reference count for a page frame
 * Used for page sharing between processes
 */
int ref_page_frame(uint64_t phys_addr) {
    uint32_t frame_num = phys_to_frame(phys_addr);

    if (!is_valid_frame(frame_num)) {
        boot_log_info("ref_page_frame: Invalid physical address");
        return -1;
    }

    page_frame_t *frame = get_frame_desc(frame_num);

    if (!frame_state_is_allocated(frame->state)) {
        boot_log_info("ref_page_frame: Page not allocated");
        return -1;
    }

    frame->ref_count++;
    return 0;
}

/* ========================================================================
 * MEMORY REGION MANAGEMENT
 * ======================================================================== */

/*
 * Add a physical memory region to the page allocator
 * Called during system initialization
 */
int add_page_alloc_region(uint64_t start_addr, uint64_t size, uint8_t type) {
    if (page_allocator.num_regions >= MAX_MEMORY_REGIONS) {
        boot_log_info("add_page_alloc_region: Too many memory regions");
        return -1;
    }

    /* Align to page boundaries */
    uint64_t aligned_start = (start_addr + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);
    uint64_t aligned_end = (start_addr + size) & ~(PAGE_SIZE_4KB - 1);

    if (aligned_end <= aligned_start) {
        boot_log_info("add_page_alloc_region: Region too small after alignment");
        return -1;
    }

    uint64_t aligned_size = aligned_end - aligned_start;
    uint32_t start_frame = phys_to_frame(aligned_start);
    uint32_t num_frames = (uint32_t)(aligned_size >> 12);

    phys_region_t *region = &page_allocator.regions[page_allocator.num_regions];
    region->start_addr = aligned_start;
    region->size = aligned_size;
    region->start_frame = start_frame;
    region->num_frames = num_frames;
    region->type = type;
    region->available = (type == EFI_CONVENTIONAL_MEMORY) ? 1 : 0;

    page_allocator.num_regions++;

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Added memory region: ");
        kprint_hex(aligned_start);
        kprint(" - ");
        kprint_hex(aligned_end);
        kprint(" (");
        kprint_decimal(num_frames);
        kprint(" frames)\n");
    });

    return 0;
}

/* ========================================================================
 * INITIALIZATION AND QUERY FUNCTIONS
 * ======================================================================== */

/*
 * Initialize the physical page frame allocator
 * Must be called after EFI memory map is parsed
 */
int init_page_allocator(void *frame_array, uint32_t max_frames) {
    page_frame_t *frames = (page_frame_t *)frame_array;

    if (!frames || max_frames == 0) {
        kernel_panic("init_page_allocator: Invalid parameters");
    }

    boot_log_debug("Initializing page frame allocator");

    page_allocator.frames = frames;
    page_allocator.total_frames = max_frames;
    page_allocator.free_frames = 0;
    page_allocator.allocated_frames = 0;
    page_allocator.reserved_frames = 0;
    page_allocator.num_regions = 0;
    page_allocator.free_list_head = INVALID_PAGE_FRAME;

    /* Initialize all frame descriptors */
    for (uint32_t i = 0; i < max_frames; i++) {
        frames[i].ref_count = 0;
        frames[i].state = PAGE_FRAME_RESERVED;
        frames[i].flags = 0;
        frames[i].order = 0;
        frames[i].next_free = INVALID_PAGE_FRAME;
    }

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Page frame allocator initialized with ");
        kprint_decimal(max_frames);
        kprint(" frame descriptors\n");
    });

    return 0;
}

/*
 * Finalize page allocator setup after all regions are added
 * Builds free lists from available memory regions
 */
int finalize_page_allocator(void) {
    boot_log_debug("Finalizing page frame allocator");

    uint32_t total_available = 0;

    /* Process all memory regions */
    for (uint32_t i = 0; i < page_allocator.num_regions; i++) {
        phys_region_t *region = &page_allocator.regions[i];

        if (!region->available) {
            continue;  /* Skip non-available regions */
        }

        /* Add all frames in this region to free list */
        for (uint32_t j = 0; j < region->num_frames; j++) {
            uint32_t frame_num = region->start_frame + j;

            if (is_valid_frame(frame_num)) {
                add_to_free_list(frame_num);
                total_available++;
            }
        }
    }

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Page allocator ready: ");
        kprint_decimal(total_available);
        kprint(" pages available\n");
    });

#ifdef PAGE_ALLOC_DEBUG
    page_alloc_debug_self_test();
#endif

    return 0;
}

/*
 * Get page allocator statistics
 */
void get_page_allocator_stats(uint32_t *total, uint32_t *free, uint32_t *allocated) {
    if (total) *total = page_allocator.total_frames;
    if (free) *free = page_allocator.free_frames;
    if (allocated) *allocated = page_allocator.allocated_frames;
}

size_t page_allocator_descriptor_size(void) {
    return sizeof(page_frame_t);
}

uint32_t page_allocator_max_supported_frames(void) {
    return MAX_PHYSICAL_PAGES;
}

/*
 * Paint every tracked physical page with a byte pattern.
 * Used by the shutdown ritual to leave a visible mark in dumps.
 */
void page_allocator_paint_all(uint8_t value) {
    if (!page_allocator.frames) {
        return;
    }

    for (uint32_t frame_num = 0; frame_num < page_allocator.total_frames; frame_num++) {
        uint64_t phys_addr = frame_to_phys(frame_num);
        uint64_t virt_addr = mm_phys_to_virt(phys_addr);
        if (!virt_addr) {
            continue;
        }

        uint8_t *ptr = (uint8_t *)virt_addr;
        for (size_t i = 0; i < PAGE_SIZE_4KB; i++) {
            ptr[i] = value;
        }
    }
}
