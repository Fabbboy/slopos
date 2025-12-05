#ifndef MM_MEMORY_LAYOUT_H
#define MM_MEMORY_LAYOUT_H

#include <stdint.h>

typedef struct kernel_memory_layout {
    uint64_t kernel_start_phys;
    uint64_t kernel_end_phys;
    uint64_t kernel_start_virt;
    uint64_t kernel_end_virt;
    uint64_t kernel_heap_start;      /* Virtual base for kernel heap */
    uint64_t kernel_heap_end;        /* Virtual end (exclusive) */
    uint64_t kernel_stack_start;
    uint64_t kernel_stack_end;
    uint64_t identity_map_end;
    uint64_t user_space_start;
    uint64_t user_space_end;
} kernel_memory_layout_t;

typedef struct process_memory_layout {
    uint64_t code_start;
    uint64_t data_start;
    uint64_t heap_start;
    uint64_t heap_max;
    uint64_t stack_top;
    uint64_t stack_size;
    uint64_t user_space_start;
    uint64_t user_space_end;
} process_memory_layout_t;

/* Kernel virtual layout anchors (single source of truth) */
#define KERNEL_HEAP_VBASE   0xFFFFFFFF90000000ULL
#define KERNEL_HEAP_SIZE    (256ULL * 1024ULL * 1024ULL) /* 256MB */

/* User-space layout defaults */
#define USER_SPACE_START_VA 0x0000000000400000ULL        /* 4MB */
#define USER_SPACE_END_VA   0x0000800000000000ULL        /* 128TB */

/* Process layout defaults */
#define PROCESS_CODE_START_VA   0x0000000000400000ULL    /* 4MB */
#define PROCESS_DATA_START_VA   0x0000000000800000ULL    /* 8MB */
#define PROCESS_HEAP_START_VA   0x0000000001000000ULL    /* 16MB */
#define PROCESS_HEAP_MAX_VA     0x0000000040000000ULL    /* 1GB window */
#define PROCESS_STACK_TOP_VA    0x00007FFFFF000000ULL
#define PROCESS_STACK_SIZE_BYTES 0x0000000000100000ULL   /* 1MB */

const kernel_memory_layout_t *get_kernel_memory_layout(void);
uint64_t mm_get_kernel_phys_start(void);
uint64_t mm_get_kernel_phys_end(void);
uint64_t mm_get_kernel_virt_start(void);
uint64_t mm_get_identity_map_limit(void);
uint64_t mm_get_kernel_heap_start(void);
uint64_t mm_get_kernel_heap_end(void);
uint64_t mm_get_user_space_start(void);
uint64_t mm_get_user_space_end(void);

const process_memory_layout_t *mm_get_process_layout(void);

#endif /* MM_MEMORY_LAYOUT_H */

