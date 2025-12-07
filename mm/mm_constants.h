/*
 * SlopOS Memory and Paging Constants
 * Shared definitions for memory layout, page tables, and limits.
 */

#ifndef MM_MM_CONSTANTS_H
#define MM_MM_CONSTANTS_H

/* Boot stack and early page tables */
#define BOOT_STACK_SIZE               0x4000   /* 16KB boot stack */
#define BOOT_STACK_PHYS_ADDR          0x20000  /* 128KB physical address */
#define EARLY_PML4_PHYS_ADDR          0x30000  /* 192KB - Page Map Level 4 */
#define EARLY_PDPT_PHYS_ADDR          0x31000  /* 196KB - Page Directory Pointer Table */
#define EARLY_PD_PHYS_ADDR            0x32000  /* 200KB - Page Directory */

/* Higher-half kernel mapping */
#define KERNEL_VIRTUAL_BASE           0xFFFFFFFF80000000ULL  /* Higher-half base */
#define KERNEL_PML4_INDEX             511      /* PML4[511] for higher-half */
#define KERNEL_PDPT_INDEX             510      /* PDPT[510] for 0x80000000 part */

/* Higher-half direct map base (HHDM). Limine provides the offset; we place the
 * mapping at this base in the virtual address space. */
#define HHDM_VIRT_BASE                0xFFFF800000000000ULL

/* Page sizes and alignment */
#define PAGE_SIZE_4KB                 0x1000   /* 4KB page */
#define PAGE_SIZE_2MB                 0x200000 /* 2MB page */
#define PAGE_SIZE_1GB                 0x40000000 /* 1GB page */
#define PAGE_ALIGN                    0x1000   /* Page alignment boundary */
#define STACK_ALIGN                   16       /* Stack alignment boundary */

/* Exception stack configuration */
#define EXCEPTION_STACK_REGION_BASE   0xFFFFFFFFB0000000ULL  /* Reserved region for IST stacks */
#define EXCEPTION_STACK_REGION_STRIDE 0x00010000ULL          /* 64KB spacing per stack */
#define EXCEPTION_STACK_GUARD_SIZE    PAGE_SIZE_4KB          /* Single guard page */
#define EXCEPTION_STACK_PAGES         8                      /* Data pages per stack (32KB) */
#define EXCEPTION_STACK_SIZE          (EXCEPTION_STACK_PAGES * PAGE_SIZE_4KB)
#define EXCEPTION_STACK_TOTAL_SIZE    (EXCEPTION_STACK_GUARD_SIZE + EXCEPTION_STACK_SIZE)

/* Page table entry flags */
#define PAGE_PRESENT                  0x001    /* Page is present in memory */
#define PAGE_WRITABLE                 0x002    /* Page is writable */
#define PAGE_USER                     0x004    /* Page accessible from user mode */
#define PAGE_WRITE_THROUGH            0x008    /* Write-through caching */
#define PAGE_CACHE_DISABLE            0x010    /* Disable caching for this page */
#define PAGE_ACCESSED                 0x020    /* Page has been accessed */
#define PAGE_DIRTY                    0x040    /* Page has been written to */
#define PAGE_SIZE                     0x080    /* Large page (2MB/1GB) */
#define PAGE_GLOBAL                   0x100    /* Global page (not flushed on CR3 reload) */

/* Combined page flags */
#define PAGE_KERNEL_RW                (PAGE_PRESENT | PAGE_WRITABLE)  /* Kernel read-write */
#define PAGE_KERNEL_RO                (PAGE_PRESENT)                  /* Kernel read-only */
#define PAGE_USER_RW                  (PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER)  /* User read-write */
#define PAGE_USER_RO                  (PAGE_PRESENT | PAGE_USER)      /* User read-only */
#define PAGE_LARGE_KERNEL_RW          (PAGE_PRESENT | PAGE_WRITABLE | PAGE_SIZE)  /* 2MB kernel page */

/* Page table sizing */
#define ENTRIES_PER_PAGE_TABLE        512      /* Entries per page table (512 * 8 = 4KB) */

/* System limits */
#define MAX_MEMORY_REGIONS            64       /* Maximum memory regions to track */
#define MAX_PROCESSES                 256      /* Maximum number of processes */
#define INVALID_PROCESS_ID            0xFFFFFFFF /* Invalid process ID value */

/* EFI constants */
#define EFI_PAGE_SIZE                 0x1000   /* EFI page size (4KB) */
#define EFI_CONVENTIONAL_MEMORY       7        /* EFI conventional memory type */

#endif /* MM_MM_CONSTANTS_H */

