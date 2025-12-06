/*
 * SlopOS Memory Management - Core Paging Infrastructure
 * Process-centric page table operations for x86_64
 * Maps/unmaps pages, handles table traversal, process isolation
 */

#include <stdint.h>
#include <stddef.h>
#include "../drivers/serial.h"
#include "../lib/klog.h"
#include "paging.h"
#include "page_alloc.h"
#include "../boot/limine_protocol.h"
#include "../boot/kernel_panic.h"
#include "phys_virt.h"
#include "memory_layout.h"
/* ========================================================================
 * PAGE TABLE CONSTANTS - Using boot/constants.h definitions
 * ======================================================================== */

/* Page table entry mask for extracting physical address */
#define PTE_ADDRESS_MASK              0x000FFFFFFFFFF000ULL

/* ========================================================================
 * PAGE TABLE STRUCTURES AND MANAGEMENT
 * ======================================================================== */

/* External references to early boot page tables from linker */
extern page_table_t early_pml4;
extern page_table_t early_pdpt;
extern page_table_t early_pd;

/* Kernel page directory - always active in higher-half */
static process_page_dir_t kernel_page_dir = {
    .pml4 = &early_pml4,
    .pml4_phys = 0,  /* Will be set during initialization */
    .ref_count = 1,
    .process_id = 0, /* Kernel process ID */
    .next = NULL
};

/* Current active page directory for running process */
static process_page_dir_t *current_page_dir = &kernel_page_dir;

static inline page_table_t *phys_to_page_table_ptr(uint64_t phys_addr) {
    return (page_table_t *)mm_phys_to_virt(phys_addr);
}

static inline int table_empty(const page_table_t *table) {
    if (!table) {
        return 1;
    }
    for (uint32_t i = 0; i < ENTRIES_PER_PAGE_TABLE; i++) {
        if (table->entries[i] & PAGE_PRESENT) {
            return 0;
        }
    }
    return 1;
}

static page_table_t *alloc_page_table(uint64_t *phys_out) {
    uint64_t phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if (!phys) {
        return NULL;
    }
    page_table_t *virt = phys_to_page_table_ptr(phys);
    if (!virt) {
        free_page_frame(phys);
        return NULL;
    }
    for (uint32_t i = 0; i < ENTRIES_PER_PAGE_TABLE; i++) {
        virt->entries[i] = 0;
    }
    if (phys_out) {
        *phys_out = phys;
    }
    return virt;
}

static inline uint64_t intermediate_flags(int user_mapping) {
    return PAGE_PRESENT | PAGE_WRITABLE | (user_mapping ? PAGE_USER : 0);
}

/* ========================================================================
 * SHARED HELPER IMPLEMENTATIONS
 * ======================================================================== */

/*
 * Copy kernel mappings from the bootstrap PML4 into the destination table.
 * New process address spaces inherit higher-half kernel identity.
 */
void paging_copy_kernel_mappings(page_table_t *dest_pml4) {
    if (!dest_pml4) {
        return;
    }

    if (!kernel_page_dir.pml4) {
        klog_raw(KLOG_INFO, "paging_copy_kernel_mappings: Kernel PML4 unavailable\n");
        return;
    }

    /* Copy kernel mappings, then clear the user half (indices 0-255). */
    for (uint32_t i = 0; i < ENTRIES_PER_PAGE_TABLE; i++) {
        dest_pml4->entries[i] = kernel_page_dir.pml4->entries[i];
    }
    for (uint32_t i = 0; i < (ENTRIES_PER_PAGE_TABLE / 2); i++) {
        dest_pml4->entries[i] = 0;
    }
}

/* ========================================================================
 * PAGE TABLE UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Extract page table indices from virtual address
 * Returns indices for traversing the 4-level page table hierarchy
 */
static inline uint16_t pml4_index(uint64_t vaddr) {
    return (vaddr >> 39) & 0x1FF;  /* Bits 47-39 */
}

static inline uint16_t pdpt_index(uint64_t vaddr) {
    return (vaddr >> 30) & 0x1FF;  /* Bits 38-30 */
}

static inline uint16_t pd_index(uint64_t vaddr) {
    return (vaddr >> 21) & 0x1FF;  /* Bits 29-21 */
}

static inline uint16_t pt_index(uint64_t vaddr) {
    return (vaddr >> 12) & 0x1FF;  /* Bits 20-12 */
}

/*
 * Extract physical address from page table entry
 * Masks out flags and returns 4KB-aligned physical address
 */
static inline uint64_t pte_address(uint64_t pte) {
    return pte & PTE_ADDRESS_MASK;
}

/*
 * Check if page table entry is present in memory
 */
static inline int pte_present(uint64_t pte) {
    return pte & PAGE_PRESENT;
}

/*
 * Check if page table entry represents a large page (2MB/1GB)
 */
static inline int pte_huge(uint64_t pte) {
    return pte & PAGE_SIZE;
}

/*
 * Validate virtual address for user space operations
 * Returns 1 if address is in valid user space range
 */
static inline int is_user_address(uint64_t vaddr) {
    return (vaddr >= mm_get_user_space_start() && vaddr < mm_get_user_space_end());
}

/*
 * Validate virtual address for kernel space operations
 * Returns 1 if address is in kernel space
 */
static inline __attribute__((unused)) int is_kernel_address(uint64_t vaddr) {
    uint64_t heap_start = mm_get_kernel_heap_start();
    uint64_t heap_end = mm_get_kernel_heap_end();
    return (vaddr >= KERNEL_VIRTUAL_BASE) ||
           (vaddr >= heap_start && vaddr < heap_end);
}

/* ========================================================================
 * TLB AND HARDWARE CONTROL FUNCTIONS
 * ======================================================================== */

/*
 * Invalidate TLB entry for a specific virtual page
 * Used after changing page table mappings
 */
static inline void invlpg(uint64_t vaddr) {
    __asm__ volatile ("invlpg (%0)" :: "r" (vaddr) : "memory");
}

/*
 * Flush entire TLB by reloading CR3
 * More expensive but ensures all TLB entries are invalidated
 */
static inline __attribute__((unused)) void flush_tlb(void) {
    uint64_t cr3;
    __asm__ volatile ("movq %%cr3, %0" : "=r" (cr3));
    __asm__ volatile ("movq %0, %%cr3" :: "r" (cr3) : "memory");
}

/*
 * Get current CR3 (page table base register)
 * Returns physical address of current PML4 table
 */
static inline uint64_t get_cr3(void) {
    uint64_t cr3;
    __asm__ volatile ("movq %%cr3, %0" : "=r" (cr3));
    return cr3;
}

/*
 * Set CR3 (page table base register)
 * Switches to specified PML4 table for process context switching
 */
static inline void set_cr3(uint64_t pml4_phys) {
    __asm__ volatile ("movq %0, %%cr3" :: "r" (pml4_phys) : "memory");
}

/* ========================================================================
 * CORE PAGE TABLE TRAVERSAL AND TRANSLATION
 * ======================================================================== */

/*
 * Convert virtual address to physical address using current page directory
 * Returns 0 if not mapped or on error
 * Supports 4KB, 2MB, and 1GB pages
 */
static uint64_t virt_to_phys_for_dir(process_page_dir_t *page_dir, uint64_t vaddr) {
    if (!page_dir || !page_dir->pml4) {
        klog_raw(KLOG_INFO, "virt_to_phys: No page directory\n");
        return 0;
    }

    uint16_t pml4_idx = pml4_index(vaddr);
    uint16_t pdpt_idx = pdpt_index(vaddr);
    uint16_t pd_idx = pd_index(vaddr);
    uint16_t pt_idx = pt_index(vaddr);

    /* Traverse PML4 -> PDPT */
    uint64_t pml4_entry = page_dir->pml4->entries[pml4_idx];
    if (!pte_present(pml4_entry)) {
        return 0;  /* Page not mapped */
    }

    /* Get PDPT table */
    page_table_t *pdpt = phys_to_page_table_ptr(pte_address(pml4_entry));
    if (!pdpt) {
        klog_raw(KLOG_INFO, "virt_to_phys: Invalid PDPT address\n");
        return 0;
    }

    uint64_t pdpt_entry = pdpt->entries[pdpt_idx];
    if (!pte_present(pdpt_entry)) {
        return 0;  /* Page not mapped */
    }

    /* Check for 1GB huge page */
    if (pte_huge(pdpt_entry)) {
        uint64_t page_offset = vaddr & (PAGE_SIZE_1GB - 1);
        return pte_address(pdpt_entry) + page_offset;
    }

    /* Traverse PDPT -> PD */
    page_table_t *pd = phys_to_page_table_ptr(pte_address(pdpt_entry));
    if (!pd) {
        klog_raw(KLOG_INFO, "virt_to_phys: Invalid PD address\n");
        return 0;
    }

    uint64_t pd_entry = pd->entries[pd_idx];
    if (!pte_present(pd_entry)) {
        return 0;  /* Page not mapped */
    }

    /* Check for 2MB large page */
    if (pte_huge(pd_entry)) {
        uint64_t page_offset = vaddr & (PAGE_SIZE_2MB - 1);
        return pte_address(pd_entry) + page_offset;
    }

    /* Traverse PD -> PT */
    page_table_t *pt = phys_to_page_table_ptr(pte_address(pd_entry));
    if (!pt) {
        klog_raw(KLOG_INFO, "virt_to_phys: Invalid PT address\n");
        return 0;
    }

    uint64_t pt_entry = pt->entries[pt_idx];
    if (!pte_present(pt_entry)) {
        return 0;  /* Page not mapped */
    }

    /* 4KB page translation */
    uint64_t page_offset = vaddr & (PAGE_SIZE_4KB - 1);
    return pte_address(pt_entry) + page_offset;
}

uint64_t virt_to_phys_in_dir(process_page_dir_t *page_dir, uint64_t vaddr) {
    return virt_to_phys_for_dir(page_dir, vaddr);
}

uint64_t virt_to_phys(uint64_t vaddr) {
    return virt_to_phys_for_dir(current_page_dir, vaddr);
}

/*
 * Convert virtual address to physical address for specific process
 * Used for cross-process memory operations
 */
uint64_t virt_to_phys_process(uint64_t vaddr, process_page_dir_t *page_dir) {
    if (!page_dir || !page_dir->pml4) {
        klog_raw(KLOG_INFO, "virt_to_phys_process: Invalid page directory\n");
        return 0;
    }

    /* Temporarily switch context for translation */
    process_page_dir_t *saved_page_dir = current_page_dir;
    current_page_dir = page_dir;

    uint64_t phys_addr = virt_to_phys(vaddr);

    /* Restore original context */
    current_page_dir = saved_page_dir;

    return phys_addr;
}

/* ========================================================================
 * PAGE MAPPING FUNCTIONS
 * ======================================================================== */

static int map_page_in_directory(process_page_dir_t *page_dir,
                                 uint64_t vaddr,
                                 uint64_t paddr,
                                 uint64_t flags,
                                 uint64_t page_size) {
    if (!page_dir || !page_dir->pml4) {
        klog_raw(KLOG_INFO, "map_page: No page directory provided\n");
        return -1;
    }

    if ((vaddr & (page_size - 1)) || (paddr & (page_size - 1))) {
        klog_raw(KLOG_INFO, "map_page: Addresses not aligned to requested size\n");
        return -1;
    }

    int user_mapping = (flags & PAGE_USER) && is_user_address(vaddr);
    uint64_t inter_flags = intermediate_flags(user_mapping);

    uint16_t pml4_idx = pml4_index(vaddr);
    uint16_t pdpt_idx = pdpt_index(vaddr);
    uint16_t pd_idx = pd_index(vaddr);
    uint16_t pt_idx = pt_index(vaddr);

    page_table_t *pml4 = page_dir->pml4;
    uint64_t pml4_entry = pml4->entries[pml4_idx];
    page_table_t *pdpt = NULL;
    uint64_t pdpt_phys = 0;

    if (!pte_present(pml4_entry)) {
        pdpt = alloc_page_table(&pdpt_phys);
        if (!pdpt) {
            klog_raw(KLOG_INFO, "map_page: Failed to allocate PDPT\n");
            return -1;
        }
        pml4->entries[pml4_idx] = pdpt_phys | inter_flags;
    } else {
        if (pte_huge(pml4_entry)) {
            klog_raw(KLOG_INFO, "map_page: PML4 entry is huge (unexpected)\n");
            return -1;
        }
        pdpt_phys = pte_address(pml4_entry);
        pdpt = phys_to_page_table_ptr(pdpt_phys);
        if (user_mapping && !(pml4_entry & PAGE_USER)) {
            pml4->entries[pml4_idx] = (pml4_entry & ~0xFFF) | inter_flags;
        }
    }

    if (!pdpt) {
        klog_raw(KLOG_INFO, "map_page: Invalid PDPT pointer\n");
        return -1;
    }

    uint64_t pdpt_entry = pdpt->entries[pdpt_idx];
    page_table_t *pd = NULL;
    uint64_t pd_phys = 0;

    if (page_size == PAGE_SIZE_1GB) {
        if (pte_present(pdpt_entry)) {
            klog_raw(KLOG_INFO, "map_page: PDPT entry already present for 1GB mapping\n");
            return -1;
        }
        pdpt->entries[pdpt_idx] = paddr | flags | PAGE_SIZE | PAGE_PRESENT;
        invlpg(vaddr);
        return 0;
    }

    if (!pte_present(pdpt_entry)) {
        pd = alloc_page_table(&pd_phys);
        if (!pd) {
            klog_raw(KLOG_INFO, "map_page: Failed to allocate PD\n");
            return -1;
        }
        pdpt->entries[pdpt_idx] = pd_phys | inter_flags;
    } else {
        if (pte_huge(pdpt_entry)) {
            klog_raw(KLOG_INFO, "map_page: PDPT entry is a huge page\n");
            return -1;
        }
        pd_phys = pte_address(pdpt_entry);
        pd = phys_to_page_table_ptr(pd_phys);
        if (user_mapping && !(pdpt_entry & PAGE_USER)) {
            pdpt->entries[pdpt_idx] = (pdpt_entry & ~0xFFF) | inter_flags;
        }
    }

    if (!pd) {
        klog_raw(KLOG_INFO, "map_page: Invalid PD pointer\n");
        return -1;
    }

    uint64_t pd_entry = pd->entries[pd_idx];
    if (page_size == PAGE_SIZE_2MB) {
        if (pte_present(pd_entry)) {
            klog_raw(KLOG_INFO, "map_page: PD entry already present for 2MB mapping\n");
            return -1;
        }
        pd->entries[pd_idx] = paddr | flags | PAGE_SIZE | PAGE_PRESENT;
        invlpg(vaddr);
        return 0;
    }

    if (!pte_present(pd_entry)) {
        uint64_t pt_phys = 0;
        page_table_t *pt = alloc_page_table(&pt_phys);
        if (!pt) {
            klog_raw(KLOG_INFO, "map_page: Failed to allocate PT\n");
            return -1;
        }
        pd->entries[pd_idx] = pt_phys | inter_flags;
        pd_entry = pd->entries[pd_idx];
    }

    if (pte_huge(pd_entry)) {
        klog_raw(KLOG_INFO, "map_page: PD entry is a large page\n");
        return -1;
    }

    page_table_t *pt = phys_to_page_table_ptr(pte_address(pd_entry));
    if (!pt) {
        klog_raw(KLOG_INFO, "map_page: Invalid PT pointer\n");
        return -1;
    }

    if (pt->entries[pt_idx] & PAGE_PRESENT) {
        klog_raw(KLOG_INFO, "map_page: Virtual address already mapped\n");
        return -1;
    }

    if (user_mapping && !(pd_entry & PAGE_USER)) {
        pd->entries[pd_idx] = (pd_entry & ~0xFFF) | inter_flags;
    }

    pt->entries[pt_idx] = paddr | (flags | PAGE_PRESENT);
    invlpg(vaddr);
    return 0;
}

int map_page_4kb_in_dir(process_page_dir_t *page_dir, uint64_t vaddr, uint64_t paddr, uint64_t flags) {
    return map_page_in_directory(page_dir, vaddr, paddr, flags, PAGE_SIZE_4KB);
}

int map_page_4kb(uint64_t vaddr, uint64_t paddr, uint64_t flags) {
    return map_page_in_directory(current_page_dir, vaddr, paddr, flags, PAGE_SIZE_4KB);
}

int map_page_2mb(uint64_t vaddr, uint64_t paddr, uint64_t flags) {
    return map_page_in_directory(current_page_dir, vaddr, paddr, flags, PAGE_SIZE_2MB);
}

static int unmap_page_in_directory(process_page_dir_t *page_dir, uint64_t vaddr) {
    if (!page_dir || !page_dir->pml4) {
        klog_raw(KLOG_INFO, "unmap_page: No page directory provided\n");
        return -1;
    }

    uint16_t pml4_idx = pml4_index(vaddr);
    uint16_t pdpt_idx = pdpt_index(vaddr);
    uint16_t pd_idx = pd_index(vaddr);
    uint16_t pt_idx = pt_index(vaddr);

    uint64_t pml4_entry = page_dir->pml4->entries[pml4_idx];
    if (!pte_present(pml4_entry)) {
        return 0;
    }

    page_table_t *pdpt = phys_to_page_table_ptr(pte_address(pml4_entry));
    uint64_t pdpt_entry = pdpt->entries[pdpt_idx];
    if (!pte_present(pdpt_entry)) {
        return 0;
    }

    if (pte_huge(pdpt_entry)) {
        uint64_t phys = pte_address(pdpt_entry);
        pdpt->entries[pdpt_idx] = 0;
        if (page_frame_can_free(phys)) {
            free_page_frame(phys);
        }
        invlpg(vaddr);
        if (table_empty(pdpt)) {
            page_table_t *pml4 = page_dir->pml4;
            pml4->entries[pml4_idx] = 0;
            if (page_frame_can_free(pte_address(pml4_entry))) {
                free_page_frame(pte_address(pml4_entry));
            }
        }
        return 0;
    }

    page_table_t *pd = phys_to_page_table_ptr(pte_address(pdpt_entry));
    uint64_t pd_entry = pd->entries[pd_idx];
    if (!pte_present(pd_entry)) {
        return 0;
    }

    if (pte_huge(pd_entry)) {
        uint64_t phys = pte_address(pd_entry);
        pd->entries[pd_idx] = 0;
        if (page_frame_can_free(phys)) {
            free_page_frame(phys);
        }
        invlpg(vaddr);
    } else {
        page_table_t *pt = phys_to_page_table_ptr(pte_address(pd_entry));
        if (!pt) {
            return -1;
        }
        if (pt->entries[pt_idx] & PAGE_PRESENT) {
            uint64_t phys = pte_address(pt->entries[pt_idx]);
            pt->entries[pt_idx] = 0;
            invlpg(vaddr);
            if (page_frame_can_free(phys)) {
                free_page_frame(phys);
            }
        }

        if (table_empty(pt)) {
            uint64_t phys_pt = pte_address(pd_entry);
            pd->entries[pd_idx] = 0;
            if (page_frame_can_free(phys_pt)) {
                free_page_frame(phys_pt);
            }
        }
    }

    if (table_empty(pd)) {
        uint64_t phys_pd = pte_address(pdpt_entry);
        pdpt->entries[pdpt_idx] = 0;
        if (page_frame_can_free(phys_pd)) {
            free_page_frame(phys_pd);
        }
    }

    if (table_empty(pdpt)) {
        page_table_t *pml4 = page_dir->pml4;
        pml4->entries[pml4_idx] = 0;
        if (page_frame_can_free(pte_address(pml4_entry))) {
            free_page_frame(pte_address(pml4_entry));
        }
    }

    return 0;
}

int unmap_page_in_dir(process_page_dir_t *page_dir, uint64_t vaddr) {
    return unmap_page_in_directory(page_dir, vaddr);
}

int unmap_page(uint64_t vaddr) {
    return unmap_page_in_directory(current_page_dir, vaddr);
}

/* ========================================================================
 * PROCESS PAGE DIRECTORY MANAGEMENT
 * ======================================================================== */

/*
 * Switch to a different process page directory
 * Used for process context switching
 */
int switch_page_directory(process_page_dir_t *page_dir) {
    if (!page_dir || !page_dir->pml4) {
        klog_raw(KLOG_INFO, "switch_page_directory: Invalid page directory\n");
        return -1;
    }

    /* Update CR3 to switch to new page directory */
    set_cr3(page_dir->pml4_phys);
    current_page_dir = page_dir;

    klog_debug("Switched to process page directory");
    return 0;
}

/*
 * Get current process page directory
 */
process_page_dir_t *get_current_page_directory(void) {
    return current_page_dir;
}

void paging_set_current_directory(process_page_dir_t *page_dir) {
    if (page_dir) {
        current_page_dir = page_dir;
    }
}

process_page_dir_t *paging_get_kernel_directory(void) {
    return &kernel_page_dir;
}

static void free_page_table_tree(process_page_dir_t *page_dir) {
    if (!page_dir || !page_dir->pml4) {
        return;
    }

    page_table_t *pml4 = page_dir->pml4;

    for (uint32_t pml4_idx = 0; pml4_idx < KERNEL_PML4_INDEX; pml4_idx++) {
        uint64_t pml4e = pml4->entries[pml4_idx];
        if (!pte_present(pml4e)) {
            continue;
        }

        page_table_t *pdpt = phys_to_page_table_ptr(pte_address(pml4e));
        for (uint32_t pdpt_idx = 0; pdpt_idx < ENTRIES_PER_PAGE_TABLE; pdpt_idx++) {
            uint64_t pdpte = pdpt->entries[pdpt_idx];
            if (!pte_present(pdpte)) {
                continue;
            }

            if (pte_huge(pdpte)) {
                uint64_t phys = pte_address(pdpte);
                if (page_frame_is_tracked(phys)) {
                    free_page_frame(phys);
                }
                continue;
            }

            page_table_t *pd = phys_to_page_table_ptr(pte_address(pdpte));
            for (uint32_t pd_idx = 0; pd_idx < ENTRIES_PER_PAGE_TABLE; pd_idx++) {
                uint64_t pde = pd->entries[pd_idx];
                if (!pte_present(pde)) {
                    continue;
                }

                if (pte_huge(pde)) {
                    uint64_t phys = pte_address(pde);
                    if (page_frame_is_tracked(phys)) {
                        free_page_frame(phys);
                    }
                    continue;
                }

                page_table_t *pt = phys_to_page_table_ptr(pte_address(pde));
                for (uint32_t pt_idx = 0; pt_idx < ENTRIES_PER_PAGE_TABLE; pt_idx++) {
                    uint64_t pte = pt->entries[pt_idx];
                if (pte_present(pte)) {
                    uint64_t phys = pte_address(pte);
                    if (page_frame_can_free(phys)) {
                        free_page_frame(phys);
                    }
                }
                }

                uint64_t phys_pd = pte_address(pde);
            if (page_frame_can_free(phys_pd)) {
                    free_page_frame(phys_pd);
                }
            }

            uint64_t phys_pdpt = pte_address(pdpte);
        if (page_frame_can_free(phys_pdpt)) {
                free_page_frame(phys_pdpt);
            }
        }

        uint64_t phys_pml4e = pte_address(pml4e);
    if (page_frame_can_free(phys_pml4e)) {
            free_page_frame(phys_pml4e);
        }
        pml4->entries[pml4_idx] = 0;
    }
}

void paging_free_user_space(process_page_dir_t *page_dir) {
    free_page_table_tree(page_dir);
}

/* ========================================================================
 * SYSTEM INITIALIZATION
 * ======================================================================== */

/*
 * Initialize paging system during early kernel boot
 * Sets up process-centric paging infrastructure
 */
void init_paging(void) {
    klog_debug("Initializing paging system");

    /* Get current CR3 value set by bootloader */
    uint64_t cr3 = get_cr3();
    kernel_page_dir.pml4_phys = cr3 & ~0xFFF;

    /* Ensure kernel PML4 pointer references the mapped table */
    page_table_t *pml4_ptr = phys_to_page_table_ptr(kernel_page_dir.pml4_phys);
    if (!pml4_ptr) {
        kernel_panic("Failed to translate kernel PML4 physical address");
    }
    kernel_page_dir.pml4 = pml4_ptr;

    /* Verify higher-half kernel mapping exists */
    uint64_t kernel_phys = virt_to_phys(KERNEL_VIRTUAL_BASE);
    if (kernel_phys == 0) {
        kernel_panic("Higher-half kernel mapping not found");
    }

    KLOG_BLOCK(KLOG_DEBUG, {
        klog_raw(KLOG_INFO, "Higher-half kernel mapping verified at ");
        klog_hex(KLOG_INFO, kernel_phys);
        klog_raw(KLOG_INFO, "\n");
    });

    /* Verify identity mapping exists for early boot hardware access */
    uint64_t identity_phys = virt_to_phys(0x100000); /* 1MB mark */
    if (identity_phys == 0x100000 || is_hhdm_available()) {
        klog_debug("Identity mapping verified");
    } else {
        klog_debug("Identity mapping not found (may be normal after early boot)");
    }

    klog_debug("Paging system initialized successfully");
}

/* ========================================================================
 * UTILITY AND QUERY FUNCTIONS
 * ======================================================================== */

/*
 * Get memory layout information for current mapping
 */
void get_memory_layout_info(uint64_t *kernel_virt_base, uint64_t *kernel_phys_base) {
    if (kernel_virt_base) {
        *kernel_virt_base = KERNEL_VIRTUAL_BASE;
    }

    if (kernel_phys_base) {
        *kernel_phys_base = virt_to_phys(KERNEL_VIRTUAL_BASE);
    }
}

/*
 * Check if a virtual address is currently mapped
 */
int is_mapped(uint64_t vaddr) {
    return virt_to_phys(vaddr) != 0;
}

/*
 * Get page size for a mapped virtual address
 * Returns page size in bytes, 0 if not mapped
 */
uint64_t get_page_size(uint64_t vaddr) {
    if (!current_page_dir || !current_page_dir->pml4) {
        return 0;
    }

    uint16_t pml4_idx = pml4_index(vaddr);
    uint16_t pdpt_idx = pdpt_index(vaddr);
    uint16_t pd_idx = pd_index(vaddr);

    /* Check PML4 entry */
    uint64_t pml4_entry = current_page_dir->pml4->entries[pml4_idx];
    if (!pte_present(pml4_entry)) {
        return 0;
    }

    /* Get PDPT table */
    page_table_t *pdpt = phys_to_page_table_ptr(pte_address(pml4_entry));
    uint64_t pdpt_entry = pdpt->entries[pdpt_idx];
    if (!pte_present(pdpt_entry)) {
        return 0;
    }

    /* Check for 1GB huge page */
    if (pte_huge(pdpt_entry)) {
        return PAGE_SIZE_1GB;
    }

    /* Get PD table */
    page_table_t *pd = phys_to_page_table_ptr(pte_address(pdpt_entry));
    uint64_t pd_entry = pd->entries[pd_idx];
    if (!pte_present(pd_entry)) {
        return 0;
    }

    /* Check for 2MB large page */
    if (pte_huge(pd_entry)) {
        return PAGE_SIZE_2MB;
    }

    /* Must be 4KB page if PT entry exists */
    return PAGE_SIZE_4KB;
}
