/*
 * SlopOS Memory Management - Kernel Heap Allocator
 * Provides kmalloc/kfree functionality for kernel memory allocation
 * Uses buddy allocator for efficient memory management
 */

#include <stdint.h>
#include <stddef.h>
#include "mm_constants.h"
#include "../drivers/serial.h"
#include "../drivers/wl_currency.h"
#include "../lib/klog.h"
#include "kernel_heap.h"
#include "page_alloc.h"
#include "paging.h"
#include "../boot/kernel_panic.h"
#include "../lib/memory.h"
#include "memory_layout.h"

/* ========================================================================
 * KERNEL HEAP CONSTANTS
 * ======================================================================== */

/* Allocation size constants */
#define MIN_ALLOC_SIZE                16        /* Minimum allocation size */
#define MAX_ALLOC_SIZE                0x100000  /* Maximum single allocation (1MB) */
#define HEAP_ALIGNMENT                8         /* Default alignment */

/* Block header magic values for debugging */
#define BLOCK_MAGIC_ALLOCATED         0xDEADBEEF
#define BLOCK_MAGIC_FREE              0xFEEDFACE

/* Heap allocation flags */
#define HEAP_FLAG_ZERO                0x01     /* Zero memory after allocation */
#define HEAP_FLAG_ATOMIC              0x02     /* Atomic allocation (no sleep) */

/* ========================================================================
 * HEAP BLOCK STRUCTURES
 * ======================================================================== */

/* Heap block header - tracks allocated and free memory blocks */
typedef struct heap_block {
    uint32_t magic;               /* Magic number for validation */
    uint32_t size;                /* Size of data area in bytes */
    uint32_t flags;               /* Block flags */
    uint32_t checksum;            /* Header checksum for corruption detection */
    struct heap_block *next;      /* Next block in free list */
    struct heap_block *prev;      /* Previous block in free list */
} heap_block_t;

/* Free list entry for different size classes */
typedef struct free_list {
    heap_block_t *head;           /* Head of free list */
    uint32_t count;               /* Number of blocks in list */
    uint32_t size_class;          /* Size class for this list */
} free_list_t;

/* Heap statistics structure is now in kernel_heap.h */

/* Kernel heap manager */
typedef struct kernel_heap {
    uint64_t start_addr;          /* Heap start virtual address */
    uint64_t end_addr;            /* Heap end virtual address */
    uint64_t current_break;       /* Current heap break */
    free_list_t free_lists[16];   /* Free lists for different sizes */
    heap_stats_t stats;           /* Heap statistics */
    uint32_t initialized;         /* Initialization flag */
} kernel_heap_t;

/* Global kernel heap instance */
static kernel_heap_t kernel_heap = {0};
static uint32_t heap_diagnostics_enabled = 1;

/* Upper bounds for each size class to aid diagnostics */
static const uint32_t size_class_thresholds[15] = {
    16, 32, 64, 128, 256, 512,
    1024, 2048, 4096, 8192, 16384,
    32768, 65536, 131072, 262144
};

/* ========================================================================
 * UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Calculate header checksum for corruption detection
 */
static uint32_t calculate_checksum(heap_block_t *block) {
    return block->magic ^ block->size ^ block->flags;
}

/*
 * Validate block header integrity
 */
static int validate_block(heap_block_t *block) {
    if (!block) {
        return 0;
    }

    /* Check magic number */
    if (block->magic != BLOCK_MAGIC_ALLOCATED &&
        block->magic != BLOCK_MAGIC_FREE) {
        return 0;
    }

    /* Check checksum */
    uint32_t expected = calculate_checksum(block);
    if (block->checksum != expected) {
        return 0;
    }

    return 1;
}

/*
 * Get size class index for allocation size
 */
static uint32_t get_size_class(uint32_t size) {
    if (size <= 16) return 0;
    if (size <= 32) return 1;
    if (size <= 64) return 2;
    if (size <= 128) return 3;
    if (size <= 256) return 4;
    if (size <= 512) return 5;
    if (size <= 1024) return 6;
    if (size <= 2048) return 7;
    if (size <= 4096) return 8;
    if (size <= 8192) return 9;
    if (size <= 16384) return 10;
    if (size <= 32768) return 11;
    if (size <= 65536) return 12;
    if (size <= 131072) return 13;
    if (size <= 262144) return 14;
    return 15;  /* Large allocations */
}

/*
 * Round up size to next power of 2 or minimum allocation size
 */
static uint32_t round_up_size(uint32_t size) {
    if (size < MIN_ALLOC_SIZE) {
        return MIN_ALLOC_SIZE;
    }

    /* Round up to next power of 2 */
    uint32_t rounded = MIN_ALLOC_SIZE;
    while (rounded < size) {
        rounded <<= 1;
    }

    return rounded;
}

/* ========================================================================
 * FREE LIST MANAGEMENT
 * ======================================================================== */

/*
 * Add block to appropriate free list
 */
static void add_to_free_list(heap_block_t *block) {
    if (!validate_block(block)) {
        klog_printf(KLOG_INFO, "add_to_free_list: Invalid block\n");
        return;
    }

    uint32_t size_class = get_size_class(block->size);
    free_list_t *list = &kernel_heap.free_lists[size_class];

    block->magic = BLOCK_MAGIC_FREE;
    block->flags = 0;
    block->checksum = calculate_checksum(block);

    /* Add to head of list */
    block->next = list->head;
    block->prev = NULL;

    if (list->head) {
        list->head->prev = block;
    }

    list->head = block;
    list->count++;

    kernel_heap.stats.free_blocks++;
    kernel_heap.stats.allocated_blocks--;
}

/*
 * Remove block from free list
 */
static void remove_from_free_list(heap_block_t *block) {
    if (!validate_block(block)) {
        klog_printf(KLOG_INFO, "remove_from_free_list: Invalid block\n");
        return;
    }

    uint32_t size_class = get_size_class(block->size);
    free_list_t *list = &kernel_heap.free_lists[size_class];

    /* Remove from linked list */
    if (block->prev) {
        block->prev->next = block->next;
    } else {
        list->head = block->next;
    }

    if (block->next) {
        block->next->prev = block->prev;
    }

    list->count--;

    block->magic = BLOCK_MAGIC_ALLOCATED;
    block->next = NULL;
    block->prev = NULL;
    block->checksum = calculate_checksum(block);

    kernel_heap.stats.allocated_blocks++;
    kernel_heap.stats.free_blocks--;
}

/*
 * Find suitable block in free lists
 */
static heap_block_t *find_free_block(uint32_t size) {
    uint32_t size_class = get_size_class(size);

    for (uint32_t i = size_class; i < 16; i++) {
        free_list_t *list = &kernel_heap.free_lists[i];
        if (list->head) {
            return list->head;
        }
    }

    return NULL;
}

/*
 * Check if there's sufficient free space in heap to satisfy a request
 * Returns the total free size available
 */
/* ========================================================================
 * HEAP EXPANSION
 * ======================================================================== */

/*
 * Expand heap by allocating more pages
 */
static int expand_heap(uint32_t min_size) {
    /* Calculate pages needed */
    uint32_t pages_needed = (min_size + PAGE_SIZE_4KB - 1) / PAGE_SIZE_4KB;

    /* Ensure minimum expansion */
    if (pages_needed < 4) {
        pages_needed = 4;
    }

    klog_printf(KLOG_DEBUG, "Expanding heap by %u pages\n", pages_needed);

    uint64_t expansion_start = kernel_heap.current_break;
    uint64_t total_bytes = (uint64_t)pages_needed * (uint64_t)PAGE_SIZE_4KB;
    uint32_t mapped_pages = 0;

    if (expansion_start >= kernel_heap.end_addr || expansion_start + total_bytes > kernel_heap.end_addr) {
        klog_info("expand_heap: Heap growth denied - would exceed heap window");
        wl_award_loss();
        return -1;
    }

    /* Allocate physical pages and map them */
    for (uint32_t i = 0; i < pages_needed; i++) {
        uint64_t phys_page = alloc_page_frame(0);
        if (!phys_page) {
            klog_info("expand_heap: Failed to allocate physical page");
            goto rollback;
        }

        uint64_t virt_page = expansion_start + (uint64_t)i * PAGE_SIZE_4KB;

        if (map_page_4kb(virt_page, phys_page, PAGE_KERNEL_RW) != 0) {
            klog_info("expand_heap: Failed to map heap page");
            free_page_frame(phys_page);
            goto rollback;
        }

        mapped_pages++;
    }

    /* Create large free block from new pages */
    uint64_t new_block_addr = expansion_start;
    uint32_t new_block_size = total_bytes - sizeof(heap_block_t);

    heap_block_t *new_block = (heap_block_t*)new_block_addr;
    new_block->magic = BLOCK_MAGIC_FREE;
    new_block->size = new_block_size;
    new_block->flags = 0;
    new_block->next = NULL;
    new_block->prev = NULL;
    new_block->checksum = calculate_checksum(new_block);

    /* Update heap break */
    kernel_heap.current_break += total_bytes;
    kernel_heap.stats.total_size += total_bytes;
    kernel_heap.stats.free_size += new_block_size;

    /* Add to free lists */
    add_to_free_list(new_block);

    return 0;

rollback:
    for (uint32_t j = 0; j < mapped_pages; j++) {
        uint64_t virt_page = expansion_start + (uint64_t)j * PAGE_SIZE_4KB;
        uint64_t mapped_phys = virt_to_phys(virt_page);
        if (mapped_phys) {
            unmap_page(virt_page);
            free_page_frame(mapped_phys);
        }
    }

    return -1;
}

/* ========================================================================
 * MEMORY ALLOCATION AND DEALLOCATION
 * ======================================================================== */

/*
 * Allocate memory from kernel heap
 * Returns pointer to allocated memory, NULL on failure
 */
void *kmalloc(size_t size) {
    if (!kernel_heap.initialized) {
        klog_printf(KLOG_INFO, "kmalloc: Heap not initialized\n");
        wl_award_loss();
        return NULL;
    }

    if (size == 0 || size > MAX_ALLOC_SIZE) {
        wl_award_loss();
        return NULL;
    }

    /* Round up size and add header overhead */
    uint32_t rounded_size = round_up_size(size);
    uint32_t total_size = rounded_size + sizeof(heap_block_t);

    /* Find suitable free block */
    heap_block_t *block = find_free_block(total_size);

    /* Expand heap if no suitable block found */
    if (!block) {
        if (expand_heap(total_size) != 0) {
            wl_award_loss();
            return NULL;
        }
        block = find_free_block(total_size);
    }

    if (!block) {
        klog_printf(KLOG_INFO, "kmalloc: No suitable block found after expansion\n");
        wl_award_loss();
        return NULL;
    }

    /* Remove from free list */
    remove_from_free_list(block);

    /* Split block if it's significantly larger */
    if (block->size > total_size + sizeof(heap_block_t) + MIN_ALLOC_SIZE) {
        /* Create new block from remainder */
        heap_block_t *new_block = (heap_block_t*)((uint8_t*)block + sizeof(heap_block_t) + rounded_size);
        new_block->magic = BLOCK_MAGIC_FREE;
        new_block->size = block->size - total_size;
        new_block->flags = 0;
        new_block->next = NULL;
        new_block->prev = NULL;
        new_block->checksum = calculate_checksum(new_block);

        /* Update original block size */
        block->size = rounded_size;
        block->checksum = calculate_checksum(block);

        /* Add remainder to free list */
        add_to_free_list(new_block);
    }

    /* Update statistics */
    kernel_heap.stats.allocated_size += block->size;
    kernel_heap.stats.free_size -= block->size;
    kernel_heap.stats.allocation_count++;

    /* Return pointer to data area */
    wl_award_win();
    return (void*)((uint8_t*)block + sizeof(heap_block_t));
}

/*
 * Allocate zeroed memory from kernel heap
 */
void *kzalloc(size_t size) {
    void *ptr = kmalloc(size);
    if (!ptr) {
        return NULL;
    }

    /* Zero the allocated memory */
    memset(ptr, 0, size);

    return ptr;
}

/*
 * Free memory to kernel heap
 */
void kfree(void *ptr) {
    if (!ptr || !kernel_heap.initialized) {
        return;
    }

    /* Get block header */
    heap_block_t *block = (heap_block_t*)((uint8_t*)ptr - sizeof(heap_block_t));

    if (!validate_block(block) || block->magic != BLOCK_MAGIC_ALLOCATED) {
        klog_printf(KLOG_INFO, "kfree: Invalid block or double free detected\n");
        wl_award_loss();
        return;
    }

    /* Update statistics */
    kernel_heap.stats.allocated_size -= block->size;
    kernel_heap.stats.free_size += block->size;
    kernel_heap.stats.free_count++;

    /* Add to free list */
    add_to_free_list(block);
    wl_award_win();
}

/* ========================================================================
 * INITIALIZATION AND DIAGNOSTICS
 * ======================================================================== */

/*
 * Initialize kernel heap
 * Sets up initial heap area and free lists
 */
int init_kernel_heap(void) {
    klog_debug("Initializing kernel heap");

    kernel_heap.start_addr = mm_get_kernel_heap_start();
    kernel_heap.end_addr = mm_get_kernel_heap_end();
    kernel_heap.current_break = kernel_heap.start_addr;

    /* Initialize free lists */
    for (uint32_t i = 0; i < 16; i++) {
        kernel_heap.free_lists[i].head = NULL;
        kernel_heap.free_lists[i].count = 0;
        kernel_heap.free_lists[i].size_class = i;
    }

    /* Initialize statistics */
    kernel_heap.stats.total_size = 0;
    kernel_heap.stats.allocated_size = 0;
    kernel_heap.stats.free_size = 0;
    kernel_heap.stats.total_blocks = 0;
    kernel_heap.stats.allocated_blocks = 0;
    kernel_heap.stats.free_blocks = 0;
    kernel_heap.stats.allocation_count = 0;
    kernel_heap.stats.free_count = 0;

    /* Perform initial heap expansion */
    if (expand_heap(PAGE_SIZE_4KB * 4) != 0) {
        kernel_panic("Failed to initialize kernel heap");
    }

    kernel_heap.initialized = 1;

    klog_printf(KLOG_DEBUG, "Kernel heap initialized at 0x%llx\n",
                (unsigned long long)kernel_heap.start_addr);

    return 0;
}

/*
 * Get kernel heap statistics
 */
void get_heap_stats(heap_stats_t *stats) {
    if (stats) {
        *stats = kernel_heap.stats;
    }
}

void kernel_heap_enable_diagnostics(int enable) {
    heap_diagnostics_enabled = (enable != 0);
}

/*
 * Print heap statistics for debugging
 */
void print_heap_stats(void) {
    klog_printf(KLOG_INFO, "=== Kernel Heap Statistics ===\n");
    klog_printf(KLOG_INFO, "Total size: %llu bytes\n",
                (unsigned long long)kernel_heap.stats.total_size);
    klog_printf(KLOG_INFO, "Allocated: %llu bytes\n",
                (unsigned long long)kernel_heap.stats.allocated_size);
    klog_printf(KLOG_INFO, "Free: %llu bytes\n",
                (unsigned long long)kernel_heap.stats.free_size);
    klog_printf(KLOG_INFO, "Allocations: %llu\n",
                (unsigned long long)kernel_heap.stats.allocation_count);
    klog_printf(KLOG_INFO, "Frees: %llu\n",
                (unsigned long long)kernel_heap.stats.free_count);

    if (!heap_diagnostics_enabled) {
        return;
    }

    klog_printf(KLOG_INFO, "Free blocks by class:\n");

    uint64_t total_free_blocks = 0;
    uint64_t largest_free_block = 0;

    for (uint32_t i = 0; i < 16; i++) {
        heap_block_t *cursor = kernel_heap.free_lists[i].head;
        uint32_t class_count = 0;

        while (cursor) {
            class_count++;
            total_free_blocks++;
            if (cursor->size > largest_free_block) {
                largest_free_block = cursor->size;
            }
            cursor = cursor->next;
        }

        if (class_count == 0) {
            continue;
        }

        if (i < 15) {
            klog_printf(KLOG_INFO, "  <= %u: %u blocks\n",
                        size_class_thresholds[i], class_count);
        } else {
            klog_printf(KLOG_INFO, "  > %u: %u blocks\n",
                        size_class_thresholds[14], class_count);
        }
    }

    klog_printf(KLOG_INFO, "Total free blocks: %llu\n",
                (unsigned long long)total_free_blocks);

    klog_printf(KLOG_INFO, "Largest free block: %llu bytes\n",
                (unsigned long long)largest_free_block);

    if (total_free_blocks > 0) {
        uint64_t average_free = 0;
        if (kernel_heap.stats.free_size > 0) {
            average_free = kernel_heap.stats.free_size / total_free_blocks;
        }
        klog_printf(KLOG_INFO, "Average free block: %llu bytes\n",
                    (unsigned long long)average_free);
    }

    if (kernel_heap.stats.free_size > 0) {
        uint64_t fragmented_bytes = kernel_heap.stats.free_size;
        if (largest_free_block < fragmented_bytes) {
            fragmented_bytes -= largest_free_block;
        } else {
            fragmented_bytes = 0;
        }

        uint64_t fragmentation_percent = 0;
        if (kernel_heap.stats.free_size > 0) {
            fragmentation_percent = (fragmented_bytes * 100) / kernel_heap.stats.free_size;
        }

        klog_printf(KLOG_INFO, "Fragmented bytes: %llu (%llu%%)\n",
                    (unsigned long long)fragmented_bytes,
                    (unsigned long long)fragmentation_percent);
    }
}
