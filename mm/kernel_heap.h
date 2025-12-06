/*
 * SlopOS Memory Management - Kernel Heap Interface
 * Exposes kmalloc/kfree for kernel subsystems that need dynamic allocation
 */

#ifndef MM_KERNEL_HEAP_H
#define MM_KERNEL_HEAP_H

#include <stddef.h>
#include <stdint.h>

void *kmalloc(size_t size);
void kfree(void *ptr);
void print_heap_stats(void);
void kernel_heap_enable_diagnostics(int enable);
int init_kernel_heap(void);

/* Heap statistics structure for test access */
typedef struct {
    uint64_t total_size;          /* Total heap size */
    uint64_t allocated_size;      /* Currently allocated bytes */
    uint64_t free_size;           /* Currently free bytes */
    uint32_t total_blocks;        /* Total number of blocks */
    uint32_t allocated_blocks;    /* Number of allocated blocks */
    uint32_t free_blocks;         /* Number of free blocks */
    uint32_t allocation_count;    /* Total allocations made */
    uint32_t free_count;          /* Total frees made */
} heap_stats_t;

void get_heap_stats(heap_stats_t *stats);

#endif /* MM_KERNEL_HEAP_H */
