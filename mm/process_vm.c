/*
 * SlopOS Memory Management - Per-Process Virtual Memory Management
 * Manages virtual memory spaces for individual processes
 * Handles process creation, destruction, and memory isolation
 */

#include <stdint.h>
#include <stddef.h>
#include "mm_constants.h"
#include "../drivers/serial.h"
#include "../lib/klog.h"
#include "process_vm.h"
#include "memory_layout.h"
#include "kernel_heap.h"
#include "page_alloc.h"
#include "paging.h"
#include "phys_virt.h"
#include "../boot/kernel_panic.h"

extern char _text_start[], _text_end[];
extern char _rodata_start[], _rodata_end[];
extern char _data_start[], _data_end[];
extern char _bss_start[], _bss_end[];
extern char _user_text_start[], _user_text_end[];
extern char _user_rodata_start[], _user_rodata_end[];
extern char _user_data_start[], _user_data_end[];

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
        klog_printf(KLOG_INFO, "alloc_vma: Failed to allocate VMA structure\n");
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

static int vma_range_valid(uint64_t start, uint64_t end) {
    if (start >= end) {
        return 0;
    }
    if ((start & (PAGE_SIZE_4KB - 1)) != 0 || (end & (PAGE_SIZE_4KB - 1)) != 0) {
        return 0;
    }
    return 1;
}

static int vma_overlaps_range(const vm_area_t *vma, uint64_t start, uint64_t end) {
    if (!vma) {
        return 0;
    }
    return (start < vma->end_addr) && (end > vma->start_addr);
}

static int map_user_range(process_page_dir_t *page_dir,
                          uint64_t start_addr,
                          uint64_t end_addr,
                          uint64_t map_flags,
                          uint32_t *pages_mapped_out) {
    if (!page_dir) {
        klog_printf(KLOG_INFO, "map_user_range: Missing page directory\n");
        return -1;
    }

    if (start_addr & (PAGE_SIZE_4KB - 1) || end_addr & (PAGE_SIZE_4KB - 1) || end_addr <= start_addr) {
        klog_printf(KLOG_INFO, "map_user_range: Unaligned or invalid range\n");
        return -1;
    }

    uint64_t current = start_addr;
    uint32_t mapped = 0;

    while (current < end_addr) {
        uint64_t phys = alloc_page_frame(ALLOC_FLAG_ZERO);
        if (!phys) {
            klog_printf(KLOG_INFO, "map_user_range: Physical allocation failed\n");
            goto rollback;
        }

        if (map_page_4kb_in_dir(page_dir, current, phys, map_flags) != 0) {
            klog_printf(KLOG_INFO, "map_user_range: Virtual mapping failed\n");
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
        uint64_t phys = virt_to_phys_in_dir(page_dir, current);
        if (phys) {
            unmap_page_in_dir(page_dir, current);
            if (page_frame_can_free(phys)) {
                free_page_frame(phys);
            }
        }
        mapped--;
    }

    if (pages_mapped_out) {
        *pages_mapped_out = 0;
    }
    return -1;
}

static void unmap_user_range(process_page_dir_t *page_dir, uint64_t start_addr, uint64_t end_addr) {
    if (end_addr <= start_addr || !page_dir) {
        return;
    }

    for (uint64_t addr = start_addr; addr < end_addr; addr += PAGE_SIZE_4KB) {
        uint64_t phys = virt_to_phys_in_dir(page_dir, addr);
        if (phys && page_frame_can_free(phys)) {
            unmap_page_in_dir(page_dir, addr);
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
    if (!process || !vma_range_valid(start, end)) {
        return -1;
    }

    vm_area_t **link = &process->vma_list;
    vm_area_t *prev = NULL;
    while (*link && (*link)->start_addr < start) {
        prev = *link;
        link = &(*link)->next;
    }

    vm_area_t *next = *link;

    if (prev && vma_overlaps_range(prev, start, end) && prev->flags != flags) {
        klog_printf(KLOG_INFO, "add_vma_to_process: Overlap with incompatible VMA\n");
        return -1;
    }

    if (next && vma_overlaps_range(next, start, end) && next->flags != flags) {
        klog_printf(KLOG_INFO, "add_vma_to_process: Overlap with incompatible next VMA\n");
        return -1;
    }

    vm_area_t *vma = alloc_vma();
    if (!vma) {
        klog_printf(KLOG_INFO, "add_vma_to_process: Failed to allocate VMA\n");
        return -1;
    }

    vma->start_addr = start;
    vma->end_addr = end;
    vma->flags = flags;

    /* Merge with previous if compatible */
    if (prev && prev->end_addr == start && prev->flags == flags) {
        prev->end_addr = end;
        free_vma(vma);
        vma = prev;
    } else {
        vma->next = next;
        *link = vma;
    }

    /* Merge with next if compatible */
    if (vma->next && vma->next->start_addr == vma->end_addr && vma->next->flags == vma->flags) {
        vm_area_t *to_merge = vma->next;
        vma->end_addr = to_merge->end_addr;
        vma->next = to_merge->next;
        free_vma(to_merge);
    }

    return 0;
}

/*
 * Remove a virtual memory area from a process
 */
static int remove_vma_from_process(process_vm_t *process, uint64_t start, uint64_t end) {
    if (!process) {
        return -1;
    }

    if (!vma_range_valid(start, end)) {
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

static vm_area_t *find_vma_covering(process_vm_t *process, uint64_t start, uint64_t end) {
    if (!process || !vma_range_valid(start, end)) {
        return NULL;
    }

    vm_area_t *cursor = process->vma_list;
    while (cursor) {
        if (cursor->start_addr <= start && cursor->end_addr >= end) {
            return cursor;
        }
        cursor = cursor->next;
    }
    return NULL;
}

static uint32_t unmap_and_free_range(process_vm_t *process, uint64_t start, uint64_t end) {
    if (!process || !process->page_dir || !vma_range_valid(start, end)) {
        return 0;
    }

    uint32_t freed = 0;
    for (uint64_t addr = start; addr < end; addr += PAGE_SIZE_4KB) {
        uint64_t phys = virt_to_phys_in_dir(process->page_dir, addr);
        if (phys) {
            int was_allocated = page_frame_can_free(phys);
            unmap_page_in_dir(process->page_dir, addr);
            /*
             * The paging layer already frees data pages (and page tables) when
             * removing mappings. Capture the count before unmapping so we can
             * keep process->total_pages in sync without double-freeing.
             */
            if (was_allocated) {
                freed++;
            }
        }
    }
    return freed;
}

static void merge_adjacent(process_vm_t *process, vm_area_t *vma) {
    if (!process || !vma) {
        return;
    }

    vm_area_t *cursor = process->vma_list;
    vm_area_t *prev = NULL;
    while (cursor && cursor != vma) {
        prev = cursor;
        cursor = cursor->next;
    }

    if (prev && prev->end_addr == vma->start_addr && prev->flags == vma->flags) {
        prev->end_addr = vma->end_addr;
        prev->next = vma->next;
        free_vma(vma);
        vma = prev;
    }

    if (vma->next && vma->next->start_addr == vma->end_addr && vma->next->flags == vma->flags) {
        vm_area_t *n = vma->next;
        vma->end_addr = n->end_addr;
        vma->next = n->next;
        free_vma(n);
    }
}

static void teardown_process_mappings(process_vm_t *process) {
    if (!process || !process->page_dir) {
        return;
    }

    vm_area_t *cursor = process->vma_list;
    while (cursor) {
        vm_area_t *next = cursor->next;
        uint32_t freed = unmap_and_free_range(process, cursor->start_addr, cursor->end_addr);
        if (process->total_pages >= freed) {
            process->total_pages -= freed;
        } else {
            process->total_pages = 0;
        }
        free_vma(cursor);
        cursor = next;
    }

    process->vma_list = NULL;
    process->heap_end = process->heap_start;
}

/* ========================================================================
 * PROCESS CREATION AND DESTRUCTION
 * ======================================================================== */

/*
 * Create a new process virtual memory space
 * Returns process ID, INVALID_PROCESS_ID on failure
 */
uint32_t create_process_vm(void) {
    const process_memory_layout_t *layout = mm_get_process_layout();

    if (vm_manager.num_processes >= MAX_PROCESSES) {
        klog_printf(KLOG_INFO, "create_process_vm: Maximum processes reached\n");
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
        klog_printf(KLOG_INFO, "create_process_vm: No free process slots available\n");
        return INVALID_PROCESS_ID;
    }

    /* Allocate new page directory */
    uint64_t pml4_phys = alloc_page_frame(0);
    if (!pml4_phys) {
        klog_printf(KLOG_INFO, "create_process_vm: Failed to allocate PML4\n");
        return INVALID_PROCESS_ID;
    }

    /* Prepare new page table */
    page_table_t *pml4 = (page_table_t *)mm_phys_to_virt(pml4_phys);
    if (!pml4) {
        klog_printf(KLOG_INFO, "create_process_vm: No HHDM/identity map available for PML4\n");
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
        klog_printf(KLOG_INFO, "create_process_vm: Failed to allocate page directory\n");
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

    /* Expose only the dedicated user sections to Ring 3 (no full kernel mapping). */
    if ((uint64_t)_user_text_start < (uint64_t)_user_text_end) {
        if (paging_mark_range_user(page_dir, (uint64_t)_user_text_start, (uint64_t)_user_text_end, 0) != 0) {
            klog_printf(KLOG_INFO, "create_process_vm: Failed to mark user_text executable range\n");
            kfree(page_dir);
            free_page_frame(pml4_phys);
            return INVALID_PROCESS_ID;
        }
    }

    if ((uint64_t)_user_rodata_start < (uint64_t)_user_rodata_end) {
        if (paging_mark_range_user(page_dir, (uint64_t)_user_rodata_start, (uint64_t)_user_rodata_end, 0) != 0) {
            klog_printf(KLOG_INFO, "create_process_vm: Failed to mark user_rodata range\n");
            kfree(page_dir);
            free_page_frame(pml4_phys);
            return INVALID_PROCESS_ID;
        }
    }

    if ((uint64_t)_user_data_start < (uint64_t)_user_data_end) {
        if (paging_mark_range_user(page_dir, (uint64_t)_user_data_start, (uint64_t)_user_data_end, 1) != 0) {
            klog_printf(KLOG_INFO, "create_process_vm: Failed to mark user_data range\n");
            kfree(page_dir);
            free_page_frame(pml4_phys);
            return INVALID_PROCESS_ID;
        }
    }

    /* Initialize process VM descriptor */
    process->process_id = process_id;
    process->page_dir = page_dir;
    process->vma_list = NULL;
    process->code_start = layout->code_start;
    process->data_start = layout->data_start;
    process->heap_start = layout->heap_start;
    process->heap_end = layout->heap_start;
    process->stack_start = layout->stack_top - layout->stack_size;
    process->stack_end = layout->stack_top;
    process->total_pages = 1;  /* PML4 page */
    process->flags = 0;
    process->next = vm_manager.process_list;

    /* Add standard VMA regions */
    if (add_vma_to_process(process, process->code_start, process->data_start,
                           VM_FLAG_READ | VM_FLAG_EXEC | VM_FLAG_USER) != 0 ||
        add_vma_to_process(process, process->data_start, process->heap_start,
                           VM_FLAG_READ | VM_FLAG_WRITE | VM_FLAG_USER) != 0 ||
        add_vma_to_process(process, process->stack_start, process->stack_end,
                           VM_FLAG_READ | VM_FLAG_WRITE | VM_FLAG_USER) != 0) {
        klog_printf(KLOG_INFO, "create_process_vm: Failed to seed initial VMAs\n");
        teardown_process_mappings(process);
        free_page_frame(process->page_dir->pml4_phys);
        kfree(process->page_dir);
        process->page_dir = NULL;
        process->process_id = INVALID_PROCESS_ID;
        return INVALID_PROCESS_ID;
    }

    uint64_t stack_map_flags = PAGE_PRESENT | PAGE_USER | PAGE_WRITABLE;
    uint32_t stack_pages = 0;
    if (map_user_range(process->page_dir, process->stack_start, process->stack_end, stack_map_flags, &stack_pages) != 0) {
        klog_printf(KLOG_INFO, "create_process_vm: Failed to map process stack\n");
        teardown_process_mappings(process);
        free_page_frame(process->page_dir->pml4_phys);
        kfree(process->page_dir);
        process->page_dir = NULL;
        process->process_id = INVALID_PROCESS_ID;
        return INVALID_PROCESS_ID;
    }

    process->total_pages += stack_pages;

    /* Add to global list */
    vm_manager.process_list = process;
    vm_manager.num_processes++;

    klog_printf(KLOG_INFO, "Created process VM space for PID %u\n", process_id);

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

    klog_printf(KLOG_INFO, "Destroying process VM space for PID %u\n", process_id);

    /* Free all mappings owned by the process */
    teardown_process_mappings(process);

    paging_free_user_space(process->page_dir);

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
    process_vm_t *process = find_process_vm(process_id);
    if (!process) {
        return 0;
    }

    const process_memory_layout_t *layout = mm_get_process_layout();

    /* Align size to page boundary */
    size = (size + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);

    /* For now, allocate from heap area */
    uint64_t start_addr = process->heap_end;
    uint64_t end_addr = start_addr + size;

    if (end_addr > layout->heap_max) {
        klog_printf(KLOG_INFO, "process_vm_alloc: Heap overflow\n");
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

    uint32_t pages_mapped = 0;
    if (map_user_range(process->page_dir, start_addr, end_addr, map_flags, &pages_mapped) != 0) {
        return 0;
    }

    process->heap_end = end_addr;

    if (add_vma_to_process(process, start_addr, end_addr, protection_flags | VM_FLAG_USER) != 0) {
        klog_printf(KLOG_INFO, "process_vm_alloc: Failed to record VMA\n");
        unmap_user_range(process->page_dir, start_addr, end_addr);
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
    if (!process || !process->page_dir || size == 0) {
        return -1;
    }

    /* Align to page boundary */
    uint64_t start = vaddr & ~(PAGE_SIZE_4KB - 1);
    uint64_t end = (vaddr + size + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);

    if (!vma_range_valid(start, end)) {
        klog_printf(KLOG_INFO, "process_vm_free: Invalid or unaligned range\n");
        return -1;
    }

    vm_area_t *vma = find_vma_covering(process, start, end);
    if (!vma) {
        klog_printf(KLOG_INFO, "process_vm_free: Range not covered by a VMA\n");
        return -1;
    }

    uint32_t freed = unmap_and_free_range(process, start, end);

    /* Adjust or remove VMA based on free range position */
    if (start == vma->start_addr && end == vma->end_addr) {
        remove_vma_from_process(process, vma->start_addr, vma->end_addr);
    } else if (start == vma->start_addr) {
        /* Trim from front */
        vma->start_addr = end;
    } else if (end == vma->end_addr) {
        /* Trim from back */
        vma->end_addr = start;
    } else {
        /* Split into two ranges */
        uint64_t right_start = end;
        uint64_t right_end = vma->end_addr;
        vma->end_addr = start;
        if (add_vma_to_process(process, right_start, right_end, vma->flags) != 0) {
            klog_printf(KLOG_INFO, "process_vm_free: Failed to create right split VMA\n");
            return -1;
        }
    }

    merge_adjacent(process, vma);

    if (process->total_pages >= freed) {
        process->total_pages -= freed;
    } else {
        process->total_pages = 0;
    }

    if (process->heap_end == end && end > process->heap_start) {
        process->heap_end = start;
    }

    return 0;
}

/* ========================================================================
 * INITIALIZATION AND QUERY FUNCTIONS
 * ======================================================================== */

/*
 * Initialize the process virtual memory manager
 */
int init_process_vm(void) {
    klog_debug("Initializing process virtual memory manager");

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

    klog_debug("Process VM manager initialized");
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
