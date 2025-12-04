/*
 * SlopOS Memory Management - Per-Process Virtual Memory Management
 * Manages virtual memory spaces for individual processes
 * Handles process creation, destruction, and memory isolation
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../drivers/serial.h"
#include "../boot/log.h"
#include "../boot/integration.h"
#include "kernel_heap.h"
#include "page_alloc.h"
#include "paging.h"
#include "phys_virt.h"
#include "../boot/kernel_panic.h"

/* ========================================================================
 * PROCESS VIRTUAL MEMORY CONSTANTS
 * ======================================================================== */

/* Process virtual memory layout */
#define PROCESS_CODE_START            0x400000        /* Process code segment (4MB) */
#define PROCESS_DATA_START            0x800000        /* Process data segment (8MB) */
#define PROCESS_HEAP_START            0x1000000       /* Process heap start (16MB) */
#define PROCESS_HEAP_MAX              0x40000000      /* Maximum heap size (1GB) */
#define PROCESS_STACK_TOP             0x7FFFFF000000ULL /* User stack top */
#define PROCESS_STACK_SIZE            0x100000        /* Default stack size (1MB) */

/* Process limits are defined in boot/constants.h */

/* Process memory allocation flags */
#define VM_FLAG_READ                  0x01   /* Page is readable */
#define VM_FLAG_WRITE                 0x02   /* Page is writable */
#define VM_FLAG_EXEC                  0x04   /* Page is executable */
#define VM_FLAG_USER                  0x08   /* Page accessible from user mode */
#define VM_FLAG_SHARED                0x10   /* Page is shared between processes */

/* ========================================================================
 * PROCESS VIRTUAL MEMORY STRUCTURES
 * ======================================================================== */

/* Virtual memory area descriptor */
typedef struct vm_area {
    uint64_t start_addr;          /* Start virtual address */
    uint64_t end_addr;            /* End virtual address */
    uint32_t flags;               /* Access flags (read/write/exec) */
    uint32_t ref_count;           /* Reference count for sharing */
    struct vm_area *next;         /* Next VMA in process */
} vm_area_t;

/* Process virtual memory descriptor */
typedef struct process_vm {
    uint32_t process_id;          /* Unique process identifier */
    process_page_dir_t *page_dir; /* Process page directory */
    vm_area_t *vma_list;          /* List of virtual memory areas */
    uint64_t code_start;          /* Process code segment start */
    uint64_t data_start;          /* Process data segment start */
    uint64_t heap_start;          /* Process heap start */
    uint64_t heap_end;            /* Current heap end */
    uint64_t stack_start;         /* Process stack start */
    uint64_t stack_end;           /* Process stack end */
    uint32_t total_pages;         /* Total allocated pages */
    uint32_t flags;               /* Process VM flags */
    struct process_vm *next;      /* Next process in global list */
} process_vm_t;

/* Global process VM manager */
typedef struct vm_manager {
    process_vm_t processes[MAX_PROCESSES];  /* Process VM descriptors */
    uint32_t num_processes;                 /* Number of active processes */
    uint32_t next_process_id;               /* Next process ID to assign */
    process_vm_t *active_process;           /* Currently active process */
    process_vm_t *process_list;             /* Head of process list */
} vm_manager_t;

/* Global VM manager instance */
static vm_manager_t vm_manager = {0};

/* ========================================================================
 * UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Allocate a new VMA structure
 * Returns pointer to VMA, NULL on failure
 */
static vm_area_t *alloc_vma(void) {
    vm_area_t *vma = (vm_area_t *)kmalloc(sizeof(vm_area_t));
    if (!vma) {
        kprint("alloc_vma: Failed to allocate VMA structure\n");
        return NULL;
    }

    vma->start_addr = 0;
    vma->end_addr = 0;
    vma->flags = 0;
    vma->ref_count = 1;
    vma->next = NULL;

    return vma;
}

/*
 * Free a VMA structure
 */
static void free_vma(vm_area_t *vma) {
    if (!vma) return;

    kfree(vma);
}

static int map_user_range(uint64_t start_addr, uint64_t end_addr, uint64_t map_flags, uint32_t *pages_mapped_out) {
    extern process_page_dir_t *get_current_page_directory(void);
    extern int switch_page_directory(process_page_dir_t *page_dir);
    
    if (start_addr & (PAGE_SIZE_4KB - 1) || end_addr & (PAGE_SIZE_4KB - 1) || end_addr <= start_addr) {
        kprint("map_user_range: Unaligned or invalid range\n");
        return -1;
    }

    /* This function should be called with the target process's page directory already active */
    /* But if not, we'll use the current one (which should be the process's during creation) */
    
    uint64_t current = start_addr;
    uint32_t mapped = 0;

    while (current < end_addr) {
        uint64_t phys = alloc_page_frame(0);
        if (!phys) {
            kprint("map_user_range: Physical allocation failed\n");
            goto rollback;
        }

        if (map_page_4kb(current, phys, map_flags) != 0) {
            kprint("map_user_range: Virtual mapping failed\n");
            free_page_frame(phys);
            goto rollback;
        }

        mapped++;
        current += PAGE_SIZE_4KB;
    }

    if (pages_mapped_out) {
        *pages_mapped_out = mapped;
    }
    return 0;

rollback:
    while (mapped > 0) {
        current -= PAGE_SIZE_4KB;
        uint64_t phys = mm_virt_to_phys(current);
        if (phys) {
            unmap_page(current);
            free_page_frame(phys);
        }
        mapped--;
    }

    if (pages_mapped_out) {
        *pages_mapped_out = 0;
    }
    return -1;
}

static void unmap_user_range(uint64_t start_addr, uint64_t end_addr) {
    if (end_addr <= start_addr) {
        return;
    }

    for (uint64_t addr = start_addr; addr < end_addr; addr += PAGE_SIZE_4KB) {
        uint64_t phys = mm_virt_to_phys(addr);
        if (phys) {
            unmap_page(addr);
            free_page_frame(phys);
        }
    }
}

/*
 * Find process VM descriptor by process ID
 */
static process_vm_t *find_process_vm(uint32_t process_id) {
    for (uint32_t i = 0; i < MAX_PROCESSES; i++) {
        if (vm_manager.processes[i].process_id == process_id) {
            return &vm_manager.processes[i];
        }
    }
    return NULL;
}

/*
 * Expose process page directory to other subsystems
 */
process_page_dir_t *process_vm_get_page_dir(uint32_t process_id) {
    process_vm_t *process = find_process_vm(process_id);
    if (!process) {
        return NULL;
    }

    return process->page_dir;
}

/* ========================================================================
 * VIRTUAL MEMORY AREA MANAGEMENT
 * ======================================================================== */

/*
 * Add a virtual memory area to a process
 */
static int add_vma_to_process(process_vm_t *process, uint64_t start, uint64_t end, uint32_t flags) {
    if (!process || start >= end) {
        return -1;
    }

    vm_area_t *vma = alloc_vma();
    if (!vma) {
        kprint("add_vma_to_process: Failed to allocate VMA\n");
        return -1;
    }

    vma->start_addr = start;
    vma->end_addr = end;
    vma->flags = flags;

    /* Add to process VMA list */
    vma->next = process->vma_list;
    process->vma_list = vma;

    return 0;
}

/*
 * Remove a virtual memory area from a process
 */
static int remove_vma_from_process(process_vm_t *process, uint64_t start, uint64_t end) {
    if (!process) {
        return -1;
    }

    vm_area_t **current = &process->vma_list;

    while (*current) {
        vm_area_t *vma = *current;

        if (vma->start_addr == start && vma->end_addr == end) {
            /* Remove from list */
            *current = vma->next;
            vma->next = NULL;
            free_vma(vma);
            return 0;
        }

        current = &vma->next;
    }

    return -1;  /* VMA not found */
}

/* ========================================================================
 * PROCESS CREATION AND DESTRUCTION
 * ======================================================================== */

/*
 * Create a new process virtual memory space
 * Returns process ID, INVALID_PROCESS_ID on failure
 */
uint32_t create_process_vm(void) {
    if (vm_manager.num_processes >= MAX_PROCESSES) {
        kprint("create_process_vm: Maximum processes reached\n");
        return INVALID_PROCESS_ID;
    }

    /* Find first free process slot */
    process_vm_t *process = NULL;
    for (uint32_t i = 0; i < MAX_PROCESSES; i++) {
        if (vm_manager.processes[i].process_id == INVALID_PROCESS_ID) {
            process = &vm_manager.processes[i];
            break;
        }
    }

    if (!process) {
        kprint("create_process_vm: No free process slots available\n");
        return INVALID_PROCESS_ID;
    }

    /* Allocate new page directory */
    uint64_t pml4_phys = alloc_page_frame(0);
    if (!pml4_phys) {
        kprint("create_process_vm: Failed to allocate PML4\n");
        return INVALID_PROCESS_ID;
    }

    /* Prepare new page table */
    page_table_t *pml4 = (page_table_t *)mm_phys_to_virt(pml4_phys);
    if (!pml4) {
        kprint("create_process_vm: No HHDM/identity map available for PML4\n");
        free_page_frame(pml4_phys);
        return INVALID_PROCESS_ID;
    }
    for (uint32_t i = 0; i < ENTRIES_PER_PAGE_TABLE; i++) {
        pml4->entries[i] = 0;
    }

    uint32_t process_id = vm_manager.next_process_id++;

    /* Allocate process page directory descriptor */
    process_page_dir_t *page_dir = (process_page_dir_t*)kmalloc(sizeof(process_page_dir_t));
    if (!page_dir) {
        kprint("create_process_vm: Failed to allocate page directory\n");
        free_page_frame(pml4_phys);
        return INVALID_PROCESS_ID;
    }

    page_dir->pml4 = pml4;
    page_dir->pml4_phys = pml4_phys;
    page_dir->ref_count = 1;
    page_dir->process_id = process_id;
    page_dir->next = NULL;

    /* Inherit kernel mappings */
    paging_copy_kernel_mappings(page_dir->pml4);

    /* Initialize process VM descriptor */
    process->process_id = process_id;
    process->page_dir = page_dir;
    process->vma_list = NULL;
    process->code_start = PROCESS_CODE_START;
    process->data_start = PROCESS_DATA_START;
    process->heap_start = PROCESS_HEAP_START;
    process->heap_end = PROCESS_HEAP_START;
    process->stack_start = PROCESS_STACK_TOP - PROCESS_STACK_SIZE;
    process->stack_end = PROCESS_STACK_TOP;
    process->total_pages = 1;  /* PML4 page */
    process->flags = 0;
    process->next = vm_manager.process_list;

    /* Add standard VMA regions */
    add_vma_to_process(process, process->code_start, process->data_start,
                       VM_FLAG_READ | VM_FLAG_EXEC | VM_FLAG_USER);
    add_vma_to_process(process, process->data_start, process->heap_start,
                       VM_FLAG_READ | VM_FLAG_WRITE | VM_FLAG_USER);
    add_vma_to_process(process, process->stack_start, process->stack_end,
                       VM_FLAG_READ | VM_FLAG_WRITE | VM_FLAG_USER);

    /* Map initial stack pages eagerly */
    /* Temporarily switch to process page directory for mapping */
    extern process_page_dir_t *get_current_page_directory(void);
    extern int switch_page_directory(process_page_dir_t *page_dir);
    process_page_dir_t *saved_page_dir = get_current_page_directory();
    
    /* Switch to process's page directory */
    if (switch_page_directory(page_dir) != 0) {
        kprint("create_process_vm: Failed to switch to process page directory\n");
        free_page_frame(page_dir->pml4_phys);
        kfree(page_dir);
        return INVALID_PROCESS_ID;
    }
    
    uint64_t stack_map_flags = PAGE_PRESENT | PAGE_USER | PAGE_WRITABLE;
    uint32_t stack_pages = 0;
    if (map_user_range(process->stack_start, process->stack_end, stack_map_flags, &stack_pages) != 0) {
        kprint("create_process_vm: Failed to map process stack\n");
        /* Switch back before cleanup */
        if (saved_page_dir) {
            switch_page_directory(saved_page_dir);
        }
        unmap_user_range(process->stack_start, process->stack_end);
        free_page_frame(process->page_dir->pml4_phys);
        kfree(process->page_dir);
        process->page_dir = NULL;
        process->process_id = INVALID_PROCESS_ID;
        return INVALID_PROCESS_ID;
    }
    
    /* Switch back to kernel page directory */
    if (saved_page_dir) {
        switch_page_directory(saved_page_dir);
    }

    process->total_pages += stack_pages;

    /* Add to global list */
    vm_manager.process_list = process;
    vm_manager.num_processes++;

    kprint("Created process VM space for PID ");
    kprint_decimal(process_id);
    kprint("\n");

    return process_id;
}

/*
 * Destroy a process virtual memory space
 * Frees all allocated pages and removes from system
 * Idempotent: safe to call multiple times for the same PID
 */
int destroy_process_vm(uint32_t process_id) {
    process_vm_t *process = find_process_vm(process_id);
    if (!process) {
        /* Already destroyed or never existed - not an error for idempotency */
        return 0;
    }

    /* Check if already destroyed (double-free protection) */
    if (process->process_id == INVALID_PROCESS_ID) {
        return 0;
    }

    kprint("Destroying process VM space for PID ");
    kprint_decimal(process_id);
    kprint("\n");

    /* Switch to process's page directory for unmapping */
    extern process_page_dir_t *get_current_page_directory(void);
    extern int switch_page_directory(process_page_dir_t *page_dir);
    process_page_dir_t *saved_page_dir = get_current_page_directory();
    
    if (process->page_dir && switch_page_directory(process->page_dir) != 0) {
        kprint("destroy_process_vm: Failed to switch to process page directory\n");
        /* Continue with cleanup even if switch fails */
    }

    /* Free all VMAs */
    vm_area_t *vma = process->vma_list;
    while (vma) {
        if (vma->flags & VM_FLAG_USER) {
            unmap_user_range(vma->start_addr, vma->end_addr);
        }
        vm_area_t *next = vma->next;
        free_vma(vma);
        vma = next;
    }
    process->vma_list = NULL;

    /* Switch back to kernel page directory */
    if (saved_page_dir && saved_page_dir != process->page_dir) {
        switch_page_directory(saved_page_dir);
    }

    /* Free page directory structures */
    if (process->page_dir) {
        if (process->page_dir->pml4_phys) {
            free_page_frame(process->page_dir->pml4_phys);
        }
        kfree(process->page_dir);
        process->page_dir = NULL;
    }

    /* Remove from process list */
    if (vm_manager.process_list == process) {
        vm_manager.process_list = process->next;
    } else {
        process_vm_t *current = vm_manager.process_list;
        while (current && current->next != process) {
            current = current->next;
        }
        if (current) {
            current->next = process->next;
        }
    }

    /* Update active_process if it points to this process */
    if (vm_manager.active_process == process) {
        vm_manager.active_process = NULL;
    }

    /* Mark process slot as free and clear state */
    process->process_id = INVALID_PROCESS_ID;
    process->vma_list = NULL;
    process->next = NULL;
    process->total_pages = 0;
    process->flags = 0;
    vm_manager.num_processes--;

    return 0;
}

/* ========================================================================
 * PROCESS MEMORY OPERATIONS
 * ======================================================================== */

/*
 * Allocate virtual memory for a process
 * Returns virtual address, 0 on failure
 */
uint64_t process_vm_alloc(uint32_t process_id, uint64_t size, uint32_t flags) {
    extern process_page_dir_t *get_current_page_directory(void);
    extern int switch_page_directory(process_page_dir_t *page_dir);
    
    process_vm_t *process = find_process_vm(process_id);
    if (!process) {
        return 0;
    }

    /* Align size to page boundary */
    size = (size + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);

    /* For now, allocate from heap area */
    uint64_t start_addr = process->heap_end;
    uint64_t end_addr = start_addr + size;

    if (end_addr > PROCESS_HEAP_MAX) {
        kprint("process_vm_alloc: Heap overflow\n");
        return 0;
    }

    uint32_t protection_flags = flags & (VM_FLAG_READ | VM_FLAG_WRITE | VM_FLAG_EXEC);

    if (protection_flags == 0) {
        protection_flags = VM_FLAG_READ | VM_FLAG_WRITE;
    }

    uint64_t map_flags = PAGE_PRESENT | PAGE_USER;
    if (protection_flags & VM_FLAG_WRITE) {
        map_flags |= PAGE_WRITABLE;
    }

    /* Switch to process's page directory for mapping */
    process_page_dir_t *saved_page_dir = get_current_page_directory();
    if (switch_page_directory(process->page_dir) != 0) {
        kprint("process_vm_alloc: Failed to switch to process page directory\n");
        return 0;
    }

    uint32_t pages_mapped = 0;
    if (map_user_range(start_addr, end_addr, map_flags, &pages_mapped) != 0) {
        /* Switch back on failure */
        if (saved_page_dir) {
            switch_page_directory(saved_page_dir);
        }
        return 0;
    }

    /* Switch back to kernel page directory */
    if (saved_page_dir) {
        switch_page_directory(saved_page_dir);
    }

    process->heap_end = end_addr;

    if (add_vma_to_process(process, start_addr, end_addr, protection_flags | VM_FLAG_USER) != 0) {
        kprint("process_vm_alloc: Failed to record VMA\n");
        /* Need to unmap with process page directory active */
        if (switch_page_directory(process->page_dir) == 0) {
            unmap_user_range(start_addr, end_addr);
            if (saved_page_dir) {
                switch_page_directory(saved_page_dir);
            }
        }
        process->heap_end = start_addr;
        return 0;
    }

    process->total_pages += pages_mapped;
    return start_addr;
}

/*
 * Free virtual memory for a process
 */
int process_vm_free(uint32_t process_id, uint64_t vaddr, uint64_t size) {
    process_vm_t *process = find_process_vm(process_id);
    if (!process) {
        return -1;
    }

    /* Align to page boundary */
    uint64_t start = vaddr & ~(PAGE_SIZE_4KB - 1);
    uint64_t end = (vaddr + size + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);

    /* Remove VMA */
    return remove_vma_from_process(process, start, end);
}

/* ========================================================================
 * INITIALIZATION AND QUERY FUNCTIONS
 * ======================================================================== */

/*
 * Initialize the process virtual memory manager
 */
int init_process_vm(void) {
    boot_log_debug("Initializing process virtual memory manager");

    vm_manager.num_processes = 0;
    vm_manager.next_process_id = 1;  /* Start from 1, 0 is kernel */
    vm_manager.active_process = NULL;
    vm_manager.process_list = NULL;

    /* Initialize all process slots */
    for (uint32_t i = 0; i < MAX_PROCESSES; i++) {
        vm_manager.processes[i].process_id = INVALID_PROCESS_ID;
        vm_manager.processes[i].page_dir = NULL;
        vm_manager.processes[i].vma_list = NULL;
        vm_manager.processes[i].total_pages = 0;
        vm_manager.processes[i].flags = 0;
        vm_manager.processes[i].next = NULL;
    }

    boot_log_debug("Process VM manager initialized");
    return 0;
}

/*
 * Get process VM statistics
 */
void get_process_vm_stats(uint32_t *total_processes, uint32_t *active_processes) {
    if (total_processes) {
        *total_processes = MAX_PROCESSES;
    }
    if (active_processes) {
        *active_processes = vm_manager.num_processes;
    }
}

/*
 * Get current active process ID
 */
uint32_t get_current_process_id(void) {
    if (vm_manager.active_process) {
        return vm_manager.active_process->process_id;
    }
    return 0;  /* Kernel process */
}
