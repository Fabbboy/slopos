/*
 * SlopOS Memory Management - Physical Page Frame Allocator
 * Manages allocation and deallocation of physical memory pages
 * Serves as the canonical physical allocator for paging, heap, and VM subsystems
 */

#include <stdint.h>
#include <stddef.h>
#include "mm_constants.h"
#include "../lib/klog.h"
#include "../drivers/serial.h"
#include "page_alloc.h"
#include "phys_virt.h"
#include "../boot/kernel_panic.h"

/* ========================================================================
 * PHYSICAL PAGE FRAME CONSTANTS
 * ======================================================================== */

/* Physical page frame states */
#define PAGE_FRAME_FREE               0x00   /* Available for allocation */
#define PAGE_FRAME_ALLOCATED          0x01   /* Currently allocated */
#define PAGE_FRAME_RESERVED           0x02   /* Reserved by system */
#define PAGE_FRAME_KERNEL             0x03   /* Kernel-only page */
#define PAGE_FRAME_DMA                0x04   /* DMA-capable page */

#define INVALID_PAGE_FRAME            0xFFFFFFFFu
#define DMA_MEMORY_LIMIT              0x01000000ULL

/* Buddy allocator max order (2^24 pages = 64GB coverage) */
#define MAX_ORDER                     24

/* ========================================================================
 * PAGE FRAME TRACKING STRUCTURES
 * ======================================================================== */

typedef struct page_frame {
    uint32_t ref_count;           /* Reference count for sharing */
    uint8_t state;                /* Page frame state */
    uint8_t flags;                /* Page frame flags */
    uint16_t order;               /* Buddy allocator order (for multi-page blocks) */
    uint32_t next_free;           /* Next free page frame (for free lists) */
} page_frame_t;

typedef struct phys_region {
    uint64_t start_addr;          /* Start physical address */
    uint64_t size;                /* Size in bytes */
    uint32_t start_frame;         /* First page frame number */
    uint32_t num_frames;          /* Number of page frames */
    uint8_t type;                 /* Memory type (from EFI) */
    uint8_t available;            /* Available for allocation */
} phys_region_t;

typedef struct page_allocator {
    page_frame_t *frames;         /* Array of page frame descriptors */
    uint32_t total_frames;        /* Total number of page frames */
    uint32_t max_supported_frames;/* Descriptor backing size */
    uint32_t free_frames;         /* Number of free page frames */
    uint32_t allocated_frames;    /* Number of allocated page frames */
    uint32_t reserved_frames;     /* Number of reserved page frames */
    phys_region_t regions[MAX_MEMORY_REGIONS];  /* Physical memory regions */
    uint32_t num_regions;         /* Number of memory regions */
    uint32_t free_lists[MAX_ORDER + 1]; /* Per-order free list heads */
    uint32_t max_order;           /* Highest usable order derived from total frames */
} page_allocator_t;

static page_allocator_t page_allocator = {0};

/* ========================================================================
 * UTILITY FUNCTIONS
 * ======================================================================== */

static inline uint32_t phys_to_frame(uint64_t phys_addr) {
    return (uint32_t)(phys_addr >> 12);  /* Divide by 4KB */
}

static inline uint64_t frame_to_phys(uint32_t frame_num) {
    return (uint64_t)frame_num << 12;  /* Multiply by 4KB */
}

static inline int is_valid_frame(uint32_t frame_num) {
    return frame_num < page_allocator.total_frames;
}

static inline page_frame_t *get_frame_desc(uint32_t frame_num) {
    if (!is_valid_frame(frame_num)) {
        return NULL;
    }
    return &page_allocator.frames[frame_num];
}

static inline uint32_t order_block_pages(uint32_t order) {
    return 1u << order;
}

static inline uint32_t flags_to_order(uint32_t flags) {
    uint32_t requested = (flags & ALLOC_FLAG_ORDER_MASK) >> ALLOC_FLAG_ORDER_SHIFT;
    if (requested > page_allocator.max_order) {
        requested = page_allocator.max_order;
    }
    return requested;
}

static uint8_t page_state_for_flags(uint32_t flags) {
    if (flags & ALLOC_FLAG_DMA) {
        return PAGE_FRAME_DMA;
    }
    if (flags & ALLOC_FLAG_KERNEL) {
        return PAGE_FRAME_KERNEL;
    }
    return PAGE_FRAME_ALLOCATED;
}

static int frame_state_is_allocated(uint8_t state) {
    return state == PAGE_FRAME_ALLOCATED ||
           state == PAGE_FRAME_KERNEL ||
           state == PAGE_FRAME_DMA;
}

static void free_lists_reset(void) {
    for (uint32_t i = 0; i <= MAX_ORDER; i++) {
        page_allocator.free_lists[i] = INVALID_PAGE_FRAME;
    }
}

/* ========================================================================
 * FREE LIST MANAGEMENT (BUDDY)
 * ======================================================================== */

static void free_list_push(uint32_t order, uint32_t frame_num) {
    page_frame_t *frame = get_frame_desc(frame_num);
    if (!frame) {
        return;
    }

    frame->next_free = page_allocator.free_lists[order];
    frame->order = order;
    frame->state = PAGE_FRAME_FREE;
    frame->flags = 0;
    frame->ref_count = 0;
    page_allocator.free_lists[order] = frame_num;
}

static int free_list_detach(uint32_t order, uint32_t target_frame) {
    uint32_t *head = &page_allocator.free_lists[order];
    uint32_t prev = INVALID_PAGE_FRAME;
    uint32_t current = *head;

    while (current != INVALID_PAGE_FRAME) {
        if (current == target_frame) {
            page_frame_t *curr_desc = get_frame_desc(current);
            uint32_t next = curr_desc ? curr_desc->next_free : INVALID_PAGE_FRAME;
            if (prev == INVALID_PAGE_FRAME) {
                *head = next;
            } else {
                page_frame_t *prev_desc = get_frame_desc(prev);
                if (prev_desc) {
                    prev_desc->next_free = next;
                }
            }
            if (curr_desc) {
                curr_desc->next_free = INVALID_PAGE_FRAME;
            }
            return 1;
        }
        prev = current;
        page_frame_t *curr_desc = get_frame_desc(current);
        current = curr_desc ? curr_desc->next_free : INVALID_PAGE_FRAME;
    }
    return 0;
}

static int block_meets_flags(uint32_t frame_num, uint32_t order, uint32_t flags) {
    uint64_t phys = frame_to_phys(frame_num);
    uint64_t span = (uint64_t)order_block_pages(order) * PAGE_SIZE_4KB;
    if ((flags & ALLOC_FLAG_DMA) && (phys + span) > DMA_MEMORY_LIMIT) {
        return 0;
    }
    return 1;
}

static uint32_t free_list_take_matching(uint32_t order, uint32_t flags) {
    uint32_t *head = &page_allocator.free_lists[order];
    uint32_t prev = INVALID_PAGE_FRAME;
    uint32_t current = *head;

    while (current != INVALID_PAGE_FRAME) {
        if (block_meets_flags(current, order, flags)) {
            /* Detach this block */
            page_frame_t *curr_desc = get_frame_desc(current);
            uint32_t next = curr_desc ? curr_desc->next_free : INVALID_PAGE_FRAME;
            if (prev == INVALID_PAGE_FRAME) {
                *head = next;
            } else {
                page_frame_t *prev_desc = get_frame_desc(prev);
                if (prev_desc) {
                    prev_desc->next_free = next;
                }
            }
            if (curr_desc) {
                curr_desc->next_free = INVALID_PAGE_FRAME;
            }

            uint32_t pages = order_block_pages(order);
            if (page_allocator.free_frames >= pages) {
                page_allocator.free_frames -= pages;
            }
            return current;
        }
        prev = current;
        page_frame_t *curr_desc = get_frame_desc(current);
        current = curr_desc ? curr_desc->next_free : INVALID_PAGE_FRAME;
    }

    return INVALID_PAGE_FRAME;
}

static void insert_block_coalescing(uint32_t frame_num, uint32_t order) {
    if (!is_valid_frame(frame_num)) {
        return;
    }

    uint32_t curr_frame = frame_num;
    uint32_t curr_order = order;

    while (curr_order < page_allocator.max_order) {
        uint32_t buddy = curr_frame ^ order_block_pages(curr_order);
        page_frame_t *buddy_desc = get_frame_desc(buddy);

        if (!buddy_desc || buddy_desc->state != PAGE_FRAME_FREE || buddy_desc->order != curr_order) {
            break;
        }

        if (!free_list_detach(curr_order, buddy)) {
            break;
        }

        uint32_t lower = (curr_frame < buddy) ? curr_frame : buddy;
        curr_frame = lower;
        curr_order++;
    }

    free_list_push(curr_order, curr_frame);
    page_allocator.free_frames += order_block_pages(curr_order);
}

/* ========================================================================
 * PAGE FRAME ALLOCATION AND DEALLOCATION
 * ======================================================================== */

static uint32_t allocate_block(uint32_t order, uint32_t flags) {
    for (uint32_t current_order = order; current_order <= page_allocator.max_order; current_order++) {
        uint32_t block = free_list_take_matching(current_order, flags);
        if (block == INVALID_PAGE_FRAME) {
            continue;
        }

        /* Split down to requested order */
        while (current_order > order) {
            current_order--;
            uint32_t buddy = block + order_block_pages(current_order);
            free_list_push(current_order, buddy);
            page_allocator.free_frames += order_block_pages(current_order);
        }

        page_frame_t *desc = get_frame_desc(block);
        if (desc) {
            desc->ref_count = 1;
            desc->flags = flags;
            desc->order = order;
            desc->state = page_state_for_flags(flags);
        }

        page_allocator.allocated_frames += order_block_pages(order);
        return block;
    }

    return INVALID_PAGE_FRAME;
}

uint64_t alloc_page_frames(uint32_t count, uint32_t flags) {
    if (count == 0) {
        return 0;
    }

    uint32_t order = 0;
    uint32_t pages = 1;
    while (pages < count && order < page_allocator.max_order) {
        pages <<= 1;
        order++;
    }

    /* Allow caller to override order explicitly */
    uint32_t flag_order = flags_to_order(flags);
    if (flag_order > order) {
        order = flag_order;
    }

    uint32_t frame_num = allocate_block(order, flags);
    if (frame_num == INVALID_PAGE_FRAME) {
        klog_info("alloc_page_frames: No suitable block available");
        return 0;
    }

    uint64_t phys_addr = frame_to_phys(frame_num);
    if (flags & ALLOC_FLAG_ZERO) {
        uint64_t span_pages = order_block_pages(order);
        for (uint64_t i = 0; i < span_pages; i++) {
            if (mm_zero_physical_page(phys_addr + (i * PAGE_SIZE_4KB)) != 0) {
                /* Roll back on failure */
                free_page_frame(phys_addr);
                return 0;
            }
        }
    }

    return phys_addr;
}

uint64_t alloc_page_frame(uint32_t flags) {
    return alloc_page_frames(1, flags);
}

int free_page_frame(uint64_t phys_addr) {
    uint32_t frame_num = phys_to_frame(phys_addr);

    if (!is_valid_frame(frame_num)) {
        klog_info("free_page_frame: Invalid physical address");
        return -1;
    }

    page_frame_t *frame = get_frame_desc(frame_num);
    if (!frame_state_is_allocated(frame->state)) {
        /* Quietly ignore duplicates or reserved frames */
        return 0;
    }

    if (frame->ref_count > 1) {
        frame->ref_count--;
        return 0;
    }

    uint32_t order = frame->order;
    uint32_t pages = order_block_pages(order);

    frame->ref_count = 0;
    frame->flags = 0;
    frame->state = PAGE_FRAME_FREE;

    page_allocator.allocated_frames =
        (page_allocator.allocated_frames > pages) ? page_allocator.allocated_frames - pages : 0;

    insert_block_coalescing(frame_num, order);
    return 0;
}

/* ========================================================================
 * MEMORY REGION MANAGEMENT
 * ======================================================================== */

int add_page_alloc_region(uint64_t start_addr, uint64_t size, uint8_t type) {
    if (page_allocator.num_regions >= MAX_MEMORY_REGIONS) {
        klog_info("add_page_alloc_region: Too many memory regions");
        return -1;
    }

    uint64_t aligned_start = (start_addr + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);
    uint64_t aligned_end = (start_addr + size) & ~(PAGE_SIZE_4KB - 1);

    if (aligned_end <= aligned_start) {
        klog_info("add_page_alloc_region: Region too small after alignment");
        return -1;
    }

    uint64_t aligned_size = aligned_end - aligned_start;
    uint32_t start_frame = phys_to_frame(aligned_start);
    uint32_t num_frames = (uint32_t)(aligned_size >> 12);

    if (!page_allocator.frames || page_allocator.total_frames == 0) {
        klog_info("add_page_alloc_region: Page allocator not initialized");
        return -1;
    }

    uint64_t last_frame = (uint64_t)start_frame + (uint64_t)num_frames;
    if (start_frame >= page_allocator.total_frames || last_frame > page_allocator.total_frames) {
        klog_printf(KLOG_INFO,
                    "add_page_alloc_region: Region exceeds descriptor coverage (frame %u of %u)\n",
                    (start_frame >= page_allocator.total_frames) ? start_frame : (uint32_t)(last_frame - 1),
                    page_allocator.total_frames);
        return -1;
    }

    phys_region_t *region = &page_allocator.regions[page_allocator.num_regions];
    region->start_addr = aligned_start;
    region->size = aligned_size;
    region->start_frame = start_frame;
    region->num_frames = num_frames;
    region->type = type;
    region->available = (type == EFI_CONVENTIONAL_MEMORY) ? 1 : 0;

    page_allocator.num_regions++;

    klog_printf(KLOG_DEBUG, "Added memory region: 0x%llx - 0x%llx (%u frames)\n",
                (unsigned long long)aligned_start,
                (unsigned long long)aligned_end,
                num_frames);

    return 0;
}

/* ========================================================================
 * INITIALIZATION AND QUERY FUNCTIONS
 * ======================================================================== */

static uint32_t derive_max_order(uint32_t total_frames) {
    uint32_t order = 0;
    while (order < MAX_ORDER && order_block_pages(order) <= total_frames) {
        order++;
    }
    if (order > 0) {
        order--; /* last valid */
    }
    return order;
}

int init_page_allocator(void *frame_array, uint32_t max_frames) {
    page_frame_t *frames = (page_frame_t *)frame_array;

    if (!frames || max_frames == 0) {
        kernel_panic("init_page_allocator: Invalid parameters");
    }

    klog_debug("Initializing page frame allocator");

    page_allocator.frames = frames;
    page_allocator.total_frames = max_frames;
    page_allocator.max_supported_frames = max_frames;
    page_allocator.free_frames = 0;
    page_allocator.allocated_frames = 0;
    page_allocator.reserved_frames = 0;
    page_allocator.num_regions = 0;
    page_allocator.max_order = derive_max_order(max_frames);

    free_lists_reset();

    for (uint32_t i = 0; i < max_frames; i++) {
        frames[i].ref_count = 0;
        frames[i].state = PAGE_FRAME_RESERVED;
        frames[i].flags = 0;
        frames[i].order = 0;
        frames[i].next_free = INVALID_PAGE_FRAME;
    }

    klog_printf(KLOG_DEBUG,
                "Page frame allocator initialized with %u frame descriptors (max order %u)\n",
                max_frames,
                page_allocator.max_order);

    return 0;
}

static void add_region_blocks(const phys_region_t *region) {
    if (!region || !region->available) {
        return;
    }

    uint32_t frame = region->start_frame;
    if (frame >= page_allocator.total_frames) {
        return;
    }

    uint32_t max_frames = page_allocator.total_frames - frame;
    uint32_t remaining = region->num_frames;
    if (remaining > max_frames) {
        remaining = max_frames;
    }

    while (remaining > 0) {
        uint32_t order = 0;
        while (order < page_allocator.max_order) {
            uint32_t block_pages = order_block_pages(order);
            if ((frame & (block_pages - 1)) != 0) {
                break;
            }
            if (block_pages > remaining) {
                break;
            }
            order++;
        }

        if (order > 0) {
            order--;
        }

        insert_block_coalescing(frame, order);
        uint32_t block_pages = order_block_pages(order);
        frame += block_pages;
        remaining -= block_pages;
    }
}

int finalize_page_allocator(void) {
    klog_debug("Finalizing page frame allocator");

    free_lists_reset();
    page_allocator.free_frames = 0;
    page_allocator.allocated_frames = 0;

    for (uint32_t i = 0; i < page_allocator.num_regions; i++) {
        add_region_blocks(&page_allocator.regions[i]);
    }

    klog_printf(KLOG_DEBUG, "Page allocator ready: %u pages available\n",
                page_allocator.free_frames);

    return 0;
}

void get_page_allocator_stats(uint32_t *total, uint32_t *free, uint32_t *allocated) {
    if (total) *total = page_allocator.total_frames;
    if (free) *free = page_allocator.free_frames;
    if (allocated) *allocated = page_allocator.allocated_frames;
}

size_t page_allocator_descriptor_size(void) {
    return sizeof(page_frame_t);
}

uint32_t page_allocator_max_supported_frames(void) {
    return page_allocator.max_supported_frames;
}

int page_frame_is_tracked(uint64_t phys_addr) {
    uint32_t frame_num = phys_to_frame(phys_addr);
    return frame_num < page_allocator.total_frames;
}

int page_frame_can_free(uint64_t phys_addr) {
    uint32_t frame_num = phys_to_frame(phys_addr);
    if (!is_valid_frame(frame_num)) {
        return 0;
    }
    page_frame_t *frame = get_frame_desc(frame_num);
    if (!frame) {
        return 0;
    }
    return frame_state_is_allocated(frame->state);
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
