/*
 * SlopOS Memory Management - Physical Page Frame Allocator
 * Manages allocation and deallocation of physical memory pages
 * Coordinates with buddy allocator for efficient memory management
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../drivers/serial.h"
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

/* ========================================================================
 * FREE LIST MANAGEMENT
 * ======================================================================== */

/*
 * Add page frame to free list
 */
static void add_to_free_list(uint32_t frame_num) {
    if (!is_valid_frame(frame_num)) {
        kprint("add_to_free_list: Invalid frame number\n");
        return;
    }

    page_frame_t *frame = get_frame_desc(frame_num);
    frame->next_free = page_allocator.free_list_head;
    page_allocator.free_list_head = frame_num;
    frame->state = PAGE_FRAME_FREE;
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
        kprint("alloc_page_frame: No free pages available\n");
        return 0;
    }

    page_frame_t *frame = get_frame_desc(frame_num);
    frame->ref_count = 1;
    frame->flags = flags;
    frame->order = 0;  /* Single page */

    uint64_t phys_addr = frame_to_phys(frame_num);

    /* Zero page if requested */
    if (flags & ALLOC_FLAG_ZERO) {
        if (mm_zero_physical_page(phys_addr) != 0) {
            frame->ref_count = 0;
            frame->flags = 0;
            frame->order = 0;
            add_to_free_list(frame_num);
            page_allocator.allocated_frames--;
            return 0;
        }
    }

    return phys_addr;
}

/*
 * Allocate multiple contiguous physical page frames
 * Returns physical address of first page, 0 on failure
 */
uint64_t alloc_page_frames(uint32_t count, uint32_t flags) {
    if (count == 0) {
        return 0;
    }

    if (count == 1) {
        return alloc_page_frame(flags);
    }

    /* For now, allocate individual pages */
    /* TODO: Implement proper contiguous allocation */
    kprint("Multi-page allocation not fully implemented\n");
    return alloc_page_frame(flags);
}

/*
 * Free a physical page frame
 * Returns 0 on success, -1 on failure
 */
int free_page_frame(uint64_t phys_addr) {
    uint32_t frame_num = phys_to_frame(phys_addr);

    if (!is_valid_frame(frame_num)) {
        kprint("free_page_frame: Invalid physical address\n");
        return -1;
    }

    page_frame_t *frame = get_frame_desc(frame_num);

    if (frame->state != PAGE_FRAME_ALLOCATED) {
        kprint("free_page_frame: Page not allocated\n");
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
    page_allocator.allocated_frames--;

    return 0;
}

/*
 * Increase reference count for a page frame
 * Used for page sharing between processes
 */
int ref_page_frame(uint64_t phys_addr) {
    uint32_t frame_num = phys_to_frame(phys_addr);

    if (!is_valid_frame(frame_num)) {
        kprint("ref_page_frame: Invalid physical address\n");
        return -1;
    }

    page_frame_t *frame = get_frame_desc(frame_num);

    if (frame->state != PAGE_FRAME_ALLOCATED) {
        kprint("ref_page_frame: Page not allocated\n");
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
        kprint("add_page_alloc_region: Too many memory regions\n");
        return -1;
    }

    /* Align to page boundaries */
    uint64_t aligned_start = (start_addr + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);
    uint64_t aligned_end = (start_addr + size) & ~(PAGE_SIZE_4KB - 1);

    if (aligned_end <= aligned_start) {
        kprint("add_page_alloc_region: Region too small after alignment\n");
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

    kprint("Added memory region: ");
    kprint_hex(aligned_start);
    kprint(" - ");
    kprint_hex(aligned_end);
    kprint(" (");
    kprint_decimal(num_frames);
    kprint(" frames)\n");

    return 0;
}

/* ========================================================================
 * INITIALIZATION AND QUERY FUNCTIONS
 * ======================================================================== */

/*
 * Initialize the physical page frame allocator
 * Must be called after EFI memory map is parsed
 */
int init_page_allocator(page_frame_t *frame_array, uint32_t max_frames) {
    if (!frame_array || max_frames == 0) {
        kernel_panic("init_page_allocator: Invalid parameters");
    }

    kprint("Initializing page frame allocator\n");

    page_allocator.frames = frame_array;
    page_allocator.total_frames = max_frames;
    page_allocator.free_frames = 0;
    page_allocator.allocated_frames = 0;
    page_allocator.reserved_frames = 0;
    page_allocator.num_regions = 0;
    page_allocator.free_list_head = INVALID_PAGE_FRAME;

    /* Initialize all frame descriptors */
    for (uint32_t i = 0; i < max_frames; i++) {
        page_allocator.frames[i].ref_count = 0;
        page_allocator.frames[i].state = PAGE_FRAME_RESERVED;
        page_allocator.frames[i].flags = 0;
        page_allocator.frames[i].order = 0;
        page_allocator.frames[i].next_free = INVALID_PAGE_FRAME;
    }

    kprint("Page frame allocator initialized with ");
    kprint_decimal(max_frames);
    kprint(" frame descriptors\n");

    return 0;
}

/*
 * Finalize page allocator setup after all regions are added
 * Builds free lists from available memory regions
 */
int finalize_page_allocator(void) {
    kprint("Finalizing page frame allocator\n");

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

    kprint("Page allocator ready: ");
    kprint_decimal(total_available);
    kprint(" pages available\n");

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
