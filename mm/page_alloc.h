#ifndef MM_PAGE_ALLOC_H
#define MM_PAGE_ALLOC_H

#include <stddef.h>
#include <stdint.h>

/*
 * Physical page allocator interface.
 * Provides low-level frame allocation used by paging, kernel heap, and VM subsystems.
 * Keeps the page allocator distinct from higher-level virtual memory mapping code.
 */

/* Allocation flags (kept small to allow packing order in upper bits) */
#define ALLOC_FLAG_ZERO        0x01  /* Zero the page after allocation */
#define ALLOC_FLAG_DMA         0x02  /* Allocate DMA-capable page (must fit under 16MB) */
#define ALLOC_FLAG_KERNEL      0x04  /* Kernel-only allocation */
#define ALLOC_FLAG_ORDER_SHIFT 8     /* Optional encoded order for multi-page requests */
#define ALLOC_FLAG_ORDER_MASK  (0x1Fu << ALLOC_FLAG_ORDER_SHIFT)

int init_page_allocator(void *frame_array, uint32_t max_frames);
int finalize_page_allocator(void);
int add_page_alloc_region(uint64_t start_addr, uint64_t size, uint8_t type);

uint64_t alloc_page_frames(uint32_t count, uint32_t flags);
uint64_t alloc_page_frame(uint32_t flags);
int free_page_frame(uint64_t phys_addr);
int page_frame_is_tracked(uint64_t phys_addr);
int page_frame_can_free(uint64_t phys_addr);

size_t page_allocator_descriptor_size(void);
uint32_t page_allocator_max_supported_frames(void);
void get_page_allocator_stats(uint32_t *total, uint32_t *free, uint32_t *allocated);
void page_allocator_paint_all(uint8_t value);

#endif /* MM_PAGE_ALLOC_H */
