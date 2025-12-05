#ifndef MM_PAGING_H
#define MM_PAGING_H

#include <stdint.h>
#include <stddef.h>

/*
 * SlopOS Memory Management - Paging Shared Definitions
 * Exposes paging data structures and helpers across the kernel
 */

#include "../boot/constants.h"

/* Page table structure - aligned as required by x86_64 */
typedef struct {
    uint64_t entries[ENTRIES_PER_PAGE_TABLE];
} __attribute__((aligned(PAGE_ALIGN))) page_table_t;

/* Process page directory structure shared between paging and process VM */
typedef struct process_page_dir {
    page_table_t *pml4;                    /* Process PML4 table */
    uint64_t pml4_phys;                    /* Physical address of PML4 */
    uint32_t ref_count;                    /* Reference count for sharing */
    uint32_t process_id;                   /* Process ID for debugging */
    struct process_page_dir *next;         /* Link for process list */
} process_page_dir_t;

/* Copy higher-half kernel mappings into a fresh PML4 */
void paging_copy_kernel_mappings(page_table_t *dest_pml4);

/* Virtual memory management primitives */
uint64_t virt_to_phys(uint64_t vaddr);
uint64_t virt_to_phys_in_dir(process_page_dir_t *page_dir, uint64_t vaddr);
int map_page_4kb_in_dir(process_page_dir_t *page_dir, uint64_t vaddr, uint64_t paddr, uint64_t flags);
int map_page_4kb(uint64_t vaddr, uint64_t paddr, uint64_t flags);
int map_page_2mb(uint64_t vaddr, uint64_t paddr, uint64_t flags);
int unmap_page_in_dir(process_page_dir_t *page_dir, uint64_t vaddr);
int unmap_page(uint64_t vaddr);
int switch_page_directory(process_page_dir_t *page_dir);
void paging_set_current_directory(process_page_dir_t *page_dir);
process_page_dir_t *paging_get_kernel_directory(void);
void paging_free_user_space(process_page_dir_t *page_dir);
process_page_dir_t *get_current_page_directory(void);
void init_paging(void);
int is_mapped(uint64_t vaddr);
uint64_t get_page_size(uint64_t vaddr);
void get_memory_layout_info(uint64_t *kernel_virt_base, uint64_t *kernel_phys_base);

#endif /* MM_PAGING_H */
