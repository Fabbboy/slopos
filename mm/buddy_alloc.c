/*
 * SlopOS Memory Management - Buddy Allocator for Physical Memory
 * Implements buddy allocation algorithm for efficient physical memory management
 * Backed by EFI memory descriptors from UEFI boot process
 */

#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include "../boot/constants.h"
#include "../drivers/serial.h"
#include "../boot/log.h"
#include "phys_virt.h"

/* Forward declarations */
void kernel_panic(const char *message);

/* ========================================================================
 * BUDDY ALLOCATOR CONSTANTS
 * ======================================================================== */

/* Buddy allocator configuration */
#define BUDDY_MAX_ORDER               12        /* Maximum allocation order (4MB) */
#define BUDDY_MIN_ORDER               0         /* Minimum allocation order (4KB) */
#define BUDDY_PAGE_SIZE               PAGE_SIZE_4KB
#define BUDDY_MAX_BLOCKS              0x100000  /* Maximum blocks to track (1M) */

/* Block states */
#define BUDDY_BLOCK_FREE              0x00      /* Block is free */
#define BUDDY_BLOCK_ALLOCATED         0x01      /* Block is allocated */
#define BUDDY_BLOCK_SPLIT             0x02      /* Block is split */
#define BUDDY_BLOCK_RESERVED          0x03      /* Block is reserved */

/* Allocation flags */
#define BUDDY_ALLOC_ZERO              0x01      /* Zero memory after allocation */
#define BUDDY_ALLOC_DMA               0x02      /* DMA-capable memory (low 16MB) */
#define BUDDY_ALLOC_KERNEL            0x04      /* Kernel-only allocation */

/* ========================================================================
 * BUDDY ALLOCATOR STRUCTURES
 * ======================================================================== */

/* Buddy block descriptor */
typedef struct buddy_block {
    uint32_t order;               /* Block order (power of 2 pages) */
    uint8_t state;                /* Block state */
    uint8_t flags;                /* Block flags */
    uint16_t reserved;            /* Reserved for alignment */
    uint32_t next_free;           /* Next free block in same order */
    uint32_t prev_free;           /* Previous free block in same order */
} buddy_block_t;

/* Free list for each order */
typedef struct buddy_free_list {
    uint32_t head;                /* Head of free list */
    uint32_t count;               /* Number of free blocks */
} buddy_free_list_t;

/* Buddy allocator zone - represents a contiguous memory region */
typedef struct buddy_zone {
    uint64_t start_addr;          /* Zone start physical address */
    uint64_t size;                /* Zone size in bytes */
    uint32_t start_block;         /* First block index in zone */
    uint32_t num_blocks;          /* Number of blocks in zone */
    buddy_free_list_t free_lists[BUDDY_MAX_ORDER + 1];  /* Free lists per order */
    uint32_t free_pages;          /* Total free pages in zone */
    uint32_t allocated_pages;     /* Total allocated pages in zone */
    uint8_t zone_type;            /* Memory type (from EFI) */
    uint8_t initialized;          /* Zone initialization flag */
} buddy_zone_t;

/* Buddy allocator global state */
typedef struct buddy_allocator {
    buddy_block_t *blocks;        /* Array of block descriptors */
    uint32_t total_blocks;        /* Total number of blocks */
    uint32_t next_block_index;    /* Next free descriptor slot */
    buddy_zone_t zones[MAX_MEMORY_REGIONS];  /* Memory zones */
    uint32_t num_zones;           /* Number of zones */
    uint64_t total_memory;        /* Total managed memory */
    uint64_t free_memory;         /* Total free memory */
    uint32_t allocation_count;    /* Total allocations made */
    uint32_t free_count;          /* Total frees made */
    uint32_t initialized;         /* Allocator initialization flag */
} buddy_allocator_t;

/* Global buddy allocator instance */
static buddy_allocator_t buddy_allocator = {0};

/* ========================================================================
 * UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Calculate buddy block index for given block and order
 */
static inline uint32_t get_buddy_index(uint32_t block_index, uint32_t order) {
    return block_index ^ (1U << order);
}

/*
 * Calculate parent block index when merging
 */
static inline uint32_t get_parent_index(uint32_t block_index, uint32_t order) {
    return block_index & ~(1U << order);
}

/*
 * Convert physical address to block index
 */
static uint32_t phys_to_block_index(uint64_t phys_addr) {
    /* Find which zone contains this address */
    for (uint32_t i = 0; i < buddy_allocator.num_zones; i++) {
        buddy_zone_t *zone = &buddy_allocator.zones[i];

        if (phys_addr >= zone->start_addr &&
            phys_addr < zone->start_addr + zone->size) {
            uint64_t offset = phys_addr - zone->start_addr;
            return zone->start_block + (uint32_t)(offset / BUDDY_PAGE_SIZE);
        }
    }

    return BUDDY_MAX_BLOCKS;  /* Invalid address */
}

/*
 * Convert block index to physical address
 */
static uint64_t block_index_to_phys(uint32_t block_index) {
    /* Find which zone contains this block */
    for (uint32_t i = 0; i < buddy_allocator.num_zones; i++) {
        buddy_zone_t *zone = &buddy_allocator.zones[i];

        if (block_index >= zone->start_block &&
            block_index < zone->start_block + zone->num_blocks) {
            uint32_t zone_offset = block_index - zone->start_block;
            return zone->start_addr + (zone_offset * BUDDY_PAGE_SIZE);
        }
    }

    return 0;  /* Invalid block index */
}

/*
 * Find zone containing block index
 */
static buddy_zone_t *find_zone_for_block(uint32_t block_index) {
    for (uint32_t i = 0; i < buddy_allocator.num_zones; i++) {
        buddy_zone_t *zone = &buddy_allocator.zones[i];

        if (block_index >= zone->start_block &&
            block_index < zone->start_block + zone->num_blocks) {
            return zone;
        }
    }

    return NULL;
}

/* ========================================================================
 * FREE LIST MANAGEMENT
 * ======================================================================== */

/*
 * Add block to free list for given order in zone
 */
static void add_to_free_list(buddy_zone_t *zone, uint32_t block_index, uint32_t order) {
    if (!zone || order > BUDDY_MAX_ORDER || block_index >= buddy_allocator.total_blocks) {
        kprint("add_to_free_list: Invalid parameters\n");
        return;
    }

    buddy_block_t *block = &buddy_allocator.blocks[block_index];
    buddy_free_list_t *free_list = &zone->free_lists[order];

    block->state = BUDDY_BLOCK_FREE;
    block->order = order;

    /* Add to head of free list */
    block->prev_free = BUDDY_MAX_BLOCKS;  /* No previous */
    block->next_free = free_list->head;

    if (free_list->head != BUDDY_MAX_BLOCKS) {
        buddy_allocator.blocks[free_list->head].prev_free = block_index;
    }

    free_list->head = block_index;
    free_list->count++;

    /* Update zone statistics */
    uint32_t pages = 1U << order;
    zone->free_pages += pages;
}

/*
 * Remove block from free list
 */
static void remove_from_free_list(buddy_zone_t *zone, uint32_t block_index, uint32_t order) {
    if (!zone || order > BUDDY_MAX_ORDER || block_index >= buddy_allocator.total_blocks) {
        kprint("remove_from_free_list: Invalid parameters\n");
        return;
    }

    buddy_block_t *block = &buddy_allocator.blocks[block_index];
    buddy_free_list_t *free_list = &zone->free_lists[order];

    /* Update linked list pointers */
    if (block->prev_free != BUDDY_MAX_BLOCKS) {
        buddy_allocator.blocks[block->prev_free].next_free = block->next_free;
    } else {
        free_list->head = block->next_free;
    }

    if (block->next_free != BUDDY_MAX_BLOCKS) {
        buddy_allocator.blocks[block->next_free].prev_free = block->prev_free;
    }

    block->next_free = BUDDY_MAX_BLOCKS;
    block->prev_free = BUDDY_MAX_BLOCKS;
    block->state = BUDDY_BLOCK_ALLOCATED;

    free_list->count--;

    /* Update zone statistics */
    uint32_t pages = 1U << order;
    zone->free_pages -= pages;
    zone->allocated_pages += pages;
}

/* ========================================================================
 * BUDDY ALLOCATION ALGORITHM
 * ======================================================================== */

/*
 * Split a block into two smaller buddies
 */
static int split_block(buddy_zone_t *zone, uint32_t block_index, uint32_t order) {
    if (!zone || order == 0 || order > BUDDY_MAX_ORDER) {
        return -1;
    }

    buddy_block_t *block = &buddy_allocator.blocks[block_index];

    /* Remove from current order free list */
    remove_from_free_list(zone, block_index, order);

    /* Create two smaller blocks */
    uint32_t new_order = order - 1;
    uint32_t buddy_index = block_index + (1U << new_order);

    /* Initialize first block (lower half) */
    block->order = new_order;
    block->state = BUDDY_BLOCK_SPLIT;

    /* Initialize buddy block (upper half) */
    if (buddy_index < buddy_allocator.total_blocks) {
        buddy_block_t *buddy = &buddy_allocator.blocks[buddy_index];
        buddy->order = new_order;
        buddy->state = BUDDY_BLOCK_FREE;
        buddy->flags = 0;

        /* Add buddy to appropriate free list */
        add_to_free_list(zone, buddy_index, new_order);
    }

    return 0;
}

/*
 * Merge block with its buddy
 */
static int merge_block(buddy_zone_t *zone, uint32_t block_index, uint32_t order) {
    if (!zone || order >= BUDDY_MAX_ORDER) {
        return -1;
    }

    uint32_t buddy_index = get_buddy_index(block_index, order);

    /* Check if buddy exists and is free */
    if (buddy_index >= buddy_allocator.total_blocks) {
        return -1;
    }

    buddy_block_t *buddy = &buddy_allocator.blocks[buddy_index];

    if (buddy->state != BUDDY_BLOCK_FREE || buddy->order != order) {
        return -1;
    }

    /* Remove buddy from free list */
    remove_from_free_list(zone, buddy_index, order);

    /* Determine parent block */
    uint32_t parent_index = get_parent_index(block_index, order);
    buddy_block_t *parent = &buddy_allocator.blocks[parent_index];

    /* Update parent block */
    parent->order = order + 1;
    parent->state = BUDDY_BLOCK_FREE;

    /* Add parent to higher order free list */
    add_to_free_list(zone, parent_index, order + 1);

    return 0;
}

/*
 * Allocate block of specified order from zone
 */
static uint32_t alloc_block_from_zone(buddy_zone_t *zone, uint32_t order) {
    if (!zone || order > BUDDY_MAX_ORDER) {
        return BUDDY_MAX_BLOCKS;
    }

    /* Try to find free block of requested order */
    for (uint32_t current_order = order; current_order <= BUDDY_MAX_ORDER; current_order++) {
        buddy_free_list_t *free_list = &zone->free_lists[current_order];

        if (free_list->head != BUDDY_MAX_BLOCKS) {
            uint32_t block_index = free_list->head;

            /* Remove from free list */
            remove_from_free_list(zone, block_index, current_order);

            /* Split down to requested order if necessary */
            while (current_order > order) {
                if (split_block(zone, block_index, current_order) != 0) {
                    return BUDDY_MAX_BLOCKS;
                }
                current_order--;
            }

            buddy_allocator.blocks[block_index].state = BUDDY_BLOCK_ALLOCATED;
            return block_index;
        }
    }

    return BUDDY_MAX_BLOCKS;  /* No suitable block found */
}

/* ========================================================================
 * PUBLIC ALLOCATION INTERFACE
 * ======================================================================== */

/*
 * Allocate physical memory using buddy allocator
 * Returns physical address, 0 on failure
 */
uint64_t buddy_alloc_pages(uint32_t num_pages, uint32_t flags) {
    if (!buddy_allocator.initialized || num_pages == 0) {
        return 0;
    }

    /* Calculate required order */
    uint32_t order = 0;
    uint32_t required_pages = 1;
    while (required_pages < num_pages && order < BUDDY_MAX_ORDER) {
        order++;
        required_pages <<= 1;
    }

    if (order > BUDDY_MAX_ORDER) {
        kprint("buddy_alloc_pages: Request too large\n");
        return 0;
    }

    /* Try to allocate from each zone */
    for (uint32_t i = 0; i < buddy_allocator.num_zones; i++) {
        buddy_zone_t *zone = &buddy_allocator.zones[i];

        if (!zone->initialized || zone->zone_type != EFI_CONVENTIONAL_MEMORY) {
            continue;
        }

        /* Check DMA requirements */
        if ((flags & BUDDY_ALLOC_DMA) && zone->start_addr >= 0x1000000) {
            continue;  /* Skip zones above 16MB for DMA */
        }

        uint32_t block_index = alloc_block_from_zone(zone, order);
        if (block_index != BUDDY_MAX_BLOCKS) {
            uint64_t phys_addr = block_index_to_phys(block_index);

            if ((flags & BUDDY_ALLOC_ZERO) && mm_zero_physical_page(phys_addr) != 0) {
                kprint("buddy_alloc_pages: Failed to zero page\n");
                return 0;
            }

            buddy_allocator.allocation_count++;
            buddy_allocator.free_memory -= required_pages * BUDDY_PAGE_SIZE;

            return phys_addr;
        }
    }

    kprint("buddy_alloc_pages: No suitable memory found\n");
    return 0;
}

/*
 * Free physical memory allocated by buddy allocator
 */
int buddy_free_pages(uint64_t phys_addr) {
    if (!buddy_allocator.initialized || phys_addr == 0) {
        return -1;
    }

    uint32_t block_index = phys_to_block_index(phys_addr);
    if (block_index >= buddy_allocator.total_blocks) {
        kprint("buddy_free_pages: Invalid physical address\n");
        return -1;
    }

    buddy_block_t *block = &buddy_allocator.blocks[block_index];
    if (block->state != BUDDY_BLOCK_ALLOCATED) {
        kprint("buddy_free_pages: Block not allocated\n");
        return -1;
    }

    buddy_zone_t *zone = find_zone_for_block(block_index);
    if (!zone) {
        kprint("buddy_free_pages: No zone found for block\n");
        return -1;
    }

    uint32_t order = block->order;
    uint32_t pages = 1U << order;

    /* Add to free list */
    add_to_free_list(zone, block_index, order);

    /* Try to merge with buddy */
    while (order < BUDDY_MAX_ORDER) {
        if (merge_block(zone, block_index, order) != 0) {
            break;  /* Cannot merge further */
        }
        order++;
        block_index = get_parent_index(block_index, order - 1);
    }

    buddy_allocator.free_count++;
    buddy_allocator.free_memory += pages * BUDDY_PAGE_SIZE;

    return 0;
}

/* ========================================================================
 * INITIALIZATION
 * ======================================================================== */

/*
 * Initialize buddy allocator with memory zones
 */
int init_buddy_allocator(buddy_block_t *block_array, uint32_t max_blocks) {
    if (!block_array || max_blocks == 0) {
        kernel_panic("init_buddy_allocator: Invalid parameters");
    }

    boot_log_debug("Initializing buddy allocator");

    buddy_allocator.blocks = block_array;
    buddy_allocator.total_blocks = max_blocks;
    buddy_allocator.next_block_index = 0;
    buddy_allocator.num_zones = 0;
    buddy_allocator.total_memory = 0;
    buddy_allocator.free_memory = 0;
    buddy_allocator.allocation_count = 0;
    buddy_allocator.free_count = 0;

    /* Initialize all block descriptors */
    for (uint32_t i = 0; i < max_blocks; i++) {
        buddy_allocator.blocks[i].order = 0;
        buddy_allocator.blocks[i].state = BUDDY_BLOCK_RESERVED;
        buddy_allocator.blocks[i].flags = 0;
        buddy_allocator.blocks[i].next_free = BUDDY_MAX_BLOCKS;
        buddy_allocator.blocks[i].prev_free = BUDDY_MAX_BLOCKS;
    }

    buddy_allocator.initialized = 1;

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Buddy allocator initialized with ");
        kprint_decimal(max_blocks);
        kprint(" block descriptors\n");
    });

    return 0;
}

/*
 * Add memory zone to buddy allocator
 */
int buddy_add_zone(uint64_t start_addr, uint64_t size, uint8_t zone_type) {
    if (!buddy_allocator.initialized) {
        return -1;
    }

    if (buddy_allocator.num_zones >= MAX_MEMORY_REGIONS) {
        boot_log_info("buddy_add_zone: Too many zones");
        return -1;
    }

    /* Align to page boundaries */
    uint64_t aligned_start = (start_addr + BUDDY_PAGE_SIZE - 1) & ~(BUDDY_PAGE_SIZE - 1);
    uint64_t aligned_end = (start_addr + size) & ~(BUDDY_PAGE_SIZE - 1);

    if (aligned_end <= aligned_start) {
        boot_log_info("buddy_add_zone: Zone too small after alignment");
        return -1;
    }

    uint64_t aligned_size = aligned_end - aligned_start;
    uint32_t num_pages = (uint32_t)(aligned_size / BUDDY_PAGE_SIZE);

    buddy_zone_t *zone = &buddy_allocator.zones[buddy_allocator.num_zones];
    zone->start_addr = aligned_start;
    zone->size = aligned_size;
    zone->start_block = buddy_allocator.next_block_index;
    zone->num_blocks = num_pages;
    zone->zone_type = zone_type;
    zone->free_pages = 0;
    zone->allocated_pages = 0;

    uint64_t new_next_index = (uint64_t)buddy_allocator.next_block_index + (uint64_t)num_pages;
    if (new_next_index > buddy_allocator.total_blocks) {
        boot_log_info("buddy_add_zone: Not enough block descriptors for zone");
        return -1;
    }
    buddy_allocator.next_block_index = (uint32_t)new_next_index;

    /* Initialize free lists */
    for (uint32_t i = 0; i <= BUDDY_MAX_ORDER; i++) {
        zone->free_lists[i].head = BUDDY_MAX_BLOCKS;
        zone->free_lists[i].count = 0;
    }

    /* Create initial free blocks */
    if (zone_type == EFI_CONVENTIONAL_MEMORY) {
        uint32_t remaining_pages = num_pages;
        uint32_t current_block = zone->start_block;

        while (remaining_pages > 0 && current_block < zone->start_block + zone->num_blocks) {
            uint32_t order = BUDDY_MAX_ORDER;
            while ((1U << order) > remaining_pages && order > 0) {
                order--;
            }

            add_to_free_list(zone, current_block, order);

            uint32_t block_pages = 1U << order;
            remaining_pages -= block_pages;
            current_block += block_pages;
        }

        buddy_allocator.free_memory += aligned_size;
    }

    buddy_allocator.total_memory += aligned_size;
    zone->initialized = 1;
    buddy_allocator.num_zones++;

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Added buddy zone: ");
        kprint_hex(aligned_start);
        kprint(" - ");
        kprint_hex(aligned_end);
        kprint(" (");
        kprint_decimal(aligned_size >> 20);
        kprint("MB)\n");
    });

    return 0;
}

/*
 * Get buddy allocator statistics
 */
void get_buddy_stats(uint64_t *total_memory, uint64_t *free_memory,
                     uint32_t *allocations, uint32_t *frees) {
    if (total_memory) *total_memory = buddy_allocator.total_memory;
    if (free_memory) *free_memory = buddy_allocator.free_memory;
    if (allocations) *allocations = buddy_allocator.allocation_count;
    if (frees) *frees = buddy_allocator.free_count;
}

size_t buddy_allocator_block_descriptor_size(void) {
    return sizeof(buddy_block_t);
}

uint32_t buddy_allocator_max_supported_blocks(void) {
    return BUDDY_MAX_BLOCKS;
}

/*
 * Helper: Paint memory with 0x69 without using standard memset
 * Used by the kernel purification ritual
 */
static void paint_memory_with_slop(void *addr, uint64_t size) {
    uint8_t *ptr = (uint8_t *)addr;
    for (uint64_t i = 0; i < size; i++) {
        ptr[i] = 0x69;
    }
}

/*
 * The Final Purification Ritual: Paint all known memory with 0x69
 *
 * When kernel_panic is invoked and all hope is lost, this function walks
 * through all buddy allocator zones and blocks, painting their memory
 * with the sacred value 0x69—a tribute to the essence of slop.
 *
 * This creates a visual memorial in memory dumps, ensuring that future
 * observers can see not empty zeros, but the vibrant proof that SlopOS
 * once existed in all its broken glory.
 */
void buddy_allocator_execute_purification(void) {
    /* Walk all zones and paint them with 0x69 */
    for (uint32_t zone_idx = 0; zone_idx < buddy_allocator.num_zones; zone_idx++) {
        buddy_zone_t *zone = &buddy_allocator.zones[zone_idx];

        if (!zone->initialized) {
            continue;
        }

        /* Paint the entire zone with 0x69 */
        uint64_t virt_addr = mm_phys_to_virt(zone->start_addr);
        if (virt_addr) {
            paint_memory_with_slop((void *)virt_addr, zone->size);
        }
    }

    /* Walk all blocks and paint them individually */
    for (uint32_t block_idx = 0; block_idx < buddy_allocator.next_block_index; block_idx++) {
        buddy_block_t *block = &buddy_allocator.blocks[block_idx];

        if (block->state == BUDDY_BLOCK_ALLOCATED || block->state == BUDDY_BLOCK_FREE) {
            /* Calculate physical address from block */
            uint64_t block_size = (1UL << block->order) * BUDDY_PAGE_SIZE;
            uint64_t phys_addr = buddy_allocator.zones[0].start_addr +
                                  (block_idx * BUDDY_PAGE_SIZE);

            uint64_t virt_addr = mm_phys_to_virt(phys_addr);
            if (virt_addr) {
                paint_memory_with_slop((void *)virt_addr, block_size);
            }
        }
    }
}
