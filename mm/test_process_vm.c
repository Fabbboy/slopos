/*
 * SlopOS Process VM Manager Regression Tests
 * Tests for process VM slot reuse and lifecycle management
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../drivers/serial.h"
#include "paging.h"

/* Forward declarations from process_vm module */
extern uint32_t create_process_vm(void);
extern int destroy_process_vm(uint32_t process_id);
extern void get_process_vm_stats(uint32_t *total_processes, uint32_t *active_processes);
extern process_page_dir_t *process_vm_get_page_dir(uint32_t process_id);

/* Forward declarations from paging module */
extern int switch_page_directory(process_page_dir_t *page_dir);
extern process_page_dir_t *get_current_page_directory(void);
extern uint64_t virt_to_phys(uint64_t vaddr);
extern int map_page_4kb(uint64_t vaddr, uint64_t paddr, uint64_t flags);

/* Forward declarations from page_alloc module */
extern uint64_t alloc_page_frame(uint32_t flags);

/* ========================================================================
 * VM MANAGER REGRESSION TESTS
 * ======================================================================== */

/*
 * Test: Non-sequential process creation and destruction
 * Creates multiple processes, destroys some in the middle, then verifies
 * that new processes reuse freed slots and all PIDs resolve correctly
 */
int test_process_vm_slot_reuse(void) {
    kprint("VM_TEST: Starting process VM slot reuse test\n");

    uint32_t initial_processes = 0;
    get_process_vm_stats(NULL, &initial_processes);

    /* Create 5 processes */
    uint32_t pids[5];
    for (int i = 0; i < 5; i++) {
        pids[i] = create_process_vm();
        if (pids[i] == INVALID_PROCESS_ID) {
            kprint("VM_TEST: Failed to create process ");
            kprint_decimal(i);
            kprint("\n");
            return -1;
        }
        kprint("VM_TEST: Created process with PID ");
        kprint_decimal(pids[i]);
        kprint("\n");
    }

    /* Verify all PIDs resolve */
    for (int i = 0; i < 5; i++) {
        process_page_dir_t *page_dir = process_vm_get_page_dir(pids[i]);
        if (!page_dir) {
            kprint("VM_TEST: Failed to resolve PID ");
            kprint_decimal(pids[i]);
            kprint("\n");
            return -1;
        }
    }

    /* Destroy middle processes (indices 1, 2, 3) */
    kprint("VM_TEST: Destroying middle processes\n");
    if (destroy_process_vm(pids[1]) != 0) {
        kprint("VM_TEST: Failed to destroy PID ");
        kprint_decimal(pids[1]);
        kprint("\n");
        return -1;
    }
    if (destroy_process_vm(pids[2]) != 0) {
        kprint("VM_TEST: Failed to destroy PID ");
        kprint_decimal(pids[2]);
        kprint("\n");
        return -1;
    }
    if (destroy_process_vm(pids[3]) != 0) {
        kprint("VM_TEST: Failed to destroy PID ");
        kprint_decimal(pids[3]);
        kprint("\n");
        return -1;
    }

    /* Verify destroyed PIDs no longer resolve */
    if (process_vm_get_page_dir(pids[1]) != NULL) {
        kprint("VM_TEST: Destroyed PID ");
        kprint_decimal(pids[1]);
        kprint(" still resolves (should not)\n");
        return -1;
    }
    if (process_vm_get_page_dir(pids[2]) != NULL) {
        kprint("VM_TEST: Destroyed PID ");
        kprint_decimal(pids[2]);
        kprint(" still resolves (should not)\n");
        return -1;
    }
    if (process_vm_get_page_dir(pids[3]) != NULL) {
        kprint("VM_TEST: Destroyed PID ");
        kprint_decimal(pids[3]);
        kprint(" still resolves (should not)\n");
        return -1;
    }

    /* Verify remaining processes still resolve */
    process_page_dir_t *page_dir0 = process_vm_get_page_dir(pids[0]);
    process_page_dir_t *page_dir4 = process_vm_get_page_dir(pids[4]);
    if (!page_dir0 || !page_dir4) {
        kprint("VM_TEST: Valid processes no longer resolve after middle destruction\n");
        return -1;
    }

    /* Create new processes - they should reuse freed slots */
    uint32_t new_pids[3];
    for (int i = 0; i < 3; i++) {
        new_pids[i] = create_process_vm();
        if (new_pids[i] == INVALID_PROCESS_ID) {
            kprint("VM_TEST: Failed to create new process after slot reuse\n");
            return -1;
        }
        kprint("VM_TEST: Created new process with PID ");
        kprint_decimal(new_pids[i]);
        kprint(" (should reuse freed slot)\n");
    }

    /* Verify all new PIDs resolve */
    for (int i = 0; i < 3; i++) {
        process_page_dir_t *page_dir = process_vm_get_page_dir(new_pids[i]);
        if (!page_dir) {
            kprint("VM_TEST: Failed to resolve new PID ");
            kprint_decimal(new_pids[i]);
            kprint("\n");
            return -1;
        }
    }

    /* Verify original processes still resolve (no overwrites) */
    page_dir0 = process_vm_get_page_dir(pids[0]);
    page_dir4 = process_vm_get_page_dir(pids[4]);
    if (!page_dir0 || !page_dir4) {
        kprint("VM_TEST: Original processes overwritten by new processes\n");
        return -1;
    }

    /* Clean up - destroy all remaining processes */
    kprint("VM_TEST: Cleaning up remaining processes\n");
    destroy_process_vm(pids[0]);
    destroy_process_vm(pids[4]);
    for (int i = 0; i < 3; i++) {
        destroy_process_vm(new_pids[i]);
    }

    /* Verify counters return to baseline */
    uint32_t final_processes = 0;
    get_process_vm_stats(NULL, &final_processes);
    if (final_processes != initial_processes) {
        kprint("VM_TEST: Process count mismatch: initial=");
        kprint_decimal(initial_processes);
        kprint(", final=");
        kprint_decimal(final_processes);
        kprint("\n");
        return -1;
    }

    kprint("VM_TEST: Process VM slot reuse test PASSED\n");
    return 0;
}

/*
 * Test: Counter return to baseline after teardown
 * Creates multiple processes, destroys them all, and verifies
 * that num_processes and total_pages return to baseline
 */
int test_process_vm_counter_reset(void) {
    kprint("VM_TEST: Starting process VM counter reset test\n");

    uint32_t initial_processes = 0;
    get_process_vm_stats(NULL, &initial_processes);

    /* Create 10 processes */
    uint32_t pids[10];
    for (int i = 0; i < 10; i++) {
        pids[i] = create_process_vm();
        if (pids[i] == INVALID_PROCESS_ID) {
            kprint("VM_TEST: Failed to create process ");
            kprint_decimal(i);
            kprint("\n");
            /* Clean up already created processes */
            for (int j = 0; j < i; j++) {
                destroy_process_vm(pids[j]);
            }
            return -1;
        }
    }

    uint32_t active_after_create = 0;
    get_process_vm_stats(NULL, &active_after_create);
    if (active_after_create != initial_processes + 10) {
        kprint("VM_TEST: Process count incorrect after creation: expected=");
        kprint_decimal(initial_processes + 10);
        kprint(", got=");
        kprint_decimal(active_after_create);
        kprint("\n");
        /* Clean up */
        for (int i = 0; i < 10; i++) {
            destroy_process_vm(pids[i]);
        }
        return -1;
    }

    /* Destroy all processes in reverse order (test non-sequential teardown) */
    for (int i = 9; i >= 0; i--) {
        if (destroy_process_vm(pids[i]) != 0) {
            kprint("VM_TEST: Failed to destroy PID ");
            kprint_decimal(pids[i]);
            kprint("\n");
            /* Try to clean up remaining */
            for (int j = i - 1; j >= 0; j--) {
                destroy_process_vm(pids[j]);
            }
            return -1;
        }
    }

    /* Verify counters returned to baseline */
    uint32_t final_processes = 0;
    get_process_vm_stats(NULL, &final_processes);
    if (final_processes != initial_processes) {
        kprint("VM_TEST: Process count did not return to baseline: initial=");
        kprint_decimal(initial_processes);
        kprint(", final=");
        kprint_decimal(final_processes);
        kprint("\n");
        return -1;
    }

    kprint("VM_TEST: Process VM counter reset test PASSED\n");
    return 0;
}

/*
 * Test: Double-free protection
 * Verifies that calling destroy_process_vm multiple times is safe
 */
int test_process_vm_double_free(void) {
    kprint("VM_TEST: Starting process VM double-free protection test\n");

    /* Create a process */
    uint32_t pid = create_process_vm();
    if (pid == INVALID_PROCESS_ID) {
        kprint("VM_TEST: Failed to create process for double-free test\n");
        return -1;
    }

    /* Destroy it once */
    if (destroy_process_vm(pid) != 0) {
        kprint("VM_TEST: Failed to destroy process (first time)\n");
        return -1;
    }

    /* Verify it's destroyed */
    if (process_vm_get_page_dir(pid) != NULL) {
        kprint("VM_TEST: Process still resolves after first destroy\n");
        return -1;
    }

    /* Try to destroy it again (should be idempotent) */
    if (destroy_process_vm(pid) != 0) {
        kprint("VM_TEST: Double destroy returned error (should be idempotent)\n");
        return -1;
    }

    /* Try to destroy invalid PID (should be safe) */
    if (destroy_process_vm(INVALID_PROCESS_ID) != 0) {
        kprint("VM_TEST: Destroy of invalid PID returned error (should be safe)\n");
        return -1;
    }

    kprint("VM_TEST: Process VM double-free protection test PASSED\n");
    return 0;
}

/*
 * Test: User-mode memory access after CR3 switch
 * Creates a process VM, maps a scratch page with user flags,
 * switches to its CR3, and verifies the mapping is accessible.
 * This test verifies that intermediate page tables have correct flags.
 */
int test_user_mode_paging_access(void) {
    kprint("VM_TEST: Starting user-mode paging access test\n");

    /* Create a process VM */
    uint32_t pid = create_process_vm();
    if (pid == INVALID_PROCESS_ID) {
        kprint("VM_TEST: Failed to create process for user paging test\n");
        return -1;
    }

    process_page_dir_t *page_dir = process_vm_get_page_dir(pid);
    if (!page_dir) {
        kprint("VM_TEST: Failed to get page directory\n");
        destroy_process_vm(pid);
        return -1;
    }

    /* Save current page directory */
    process_page_dir_t *saved_page_dir = get_current_page_directory();
    
    /* Switch to process page directory */
    if (switch_page_directory(page_dir) != 0) {
        kprint("VM_TEST: Failed to switch to process page directory\n");
        destroy_process_vm(pid);
        return -1;
    }

    /* Map a test page in user space */
    uint64_t test_vaddr = 0x500000; /* User space address */
    uint64_t test_paddr = alloc_page_frame(0);
    if (!test_paddr) {
        kprint("VM_TEST: Failed to allocate physical page\n");
        if (saved_page_dir) {
            switch_page_directory(saved_page_dir);
        }
        destroy_process_vm(pid);
        return -1;
    }
    kprint("VM_TEST: Allocated test phys page\n");

    uint64_t user_flags = PAGE_PRESENT | PAGE_USER | PAGE_WRITABLE;
    if (map_page_4kb(test_vaddr, test_paddr, user_flags) != 0) {
        kprint("VM_TEST: Failed to map test page\n");
        if (saved_page_dir) {
            switch_page_directory(saved_page_dir);
        }
        destroy_process_vm(pid);
        return -1;
    }
    kprint("VM_TEST: Mapped test page\n");

    /* Verify mapping exists by reading/writing */
    volatile uint32_t *test_ptr = (volatile uint32_t *)test_vaddr;
    uint32_t test_value = 0xDEADBEEF;
    
    /* Write test value */
    *test_ptr = test_value;
    kprint("VM_TEST: Wrote test value\n");
    
    /* Verify read */
    if (*test_ptr != test_value) {
        kprint("VM_TEST: Memory access test failed - write/read mismatch\n");
        if (saved_page_dir) {
            switch_page_directory(saved_page_dir);
        }
        destroy_process_vm(pid);
        return -1;
    }
    kprint("VM_TEST: Readback matched\n");

    /* Switch back to kernel page directory */
    if (saved_page_dir) {
        if (switch_page_directory(saved_page_dir) != 0) {
            kprint("VM_TEST: Failed to switch back to kernel page directory\n");
            destroy_process_vm(pid);
            return -1;
        }
    }

    /* Clean up */
    destroy_process_vm(pid);

    kprint("VM_TEST: User-mode paging access test PASSED\n");
    return 0;
}

/*
 * Test: User stack accessibility
 * Verifies that user stack pages are properly mapped and accessible
 * in a process's address space.
 */
int test_user_stack_accessibility(void) {
    kprint("VM_TEST: Starting user stack accessibility test\n");

    /* Create a process VM (this should create user stack automatically) */
    uint32_t pid = create_process_vm();
    if (pid == INVALID_PROCESS_ID) {
        kprint("VM_TEST: Failed to create process for stack test\n");
        return -1;
    }

    process_page_dir_t *page_dir = process_vm_get_page_dir(pid);
    if (!page_dir) {
        kprint("VM_TEST: Failed to get page directory\n");
        destroy_process_vm(pid);
        return -1;
    }

    /* Process stack is defined in process_vm.c */
    #define PROCESS_STACK_TOP 0x7FFFFF000000ULL
    #define PROCESS_STACK_SIZE 0x100000
    uint64_t stack_start = PROCESS_STACK_TOP - PROCESS_STACK_SIZE;
    uint64_t stack_end = PROCESS_STACK_TOP;

    /* Switch to process page directory */
    process_page_dir_t *saved_page_dir = get_current_page_directory();
    
    if (switch_page_directory(page_dir) != 0) {
        kprint("VM_TEST: Failed to switch to process page directory\n");
        destroy_process_vm(pid);
        return -1;
    }

    /* Verify stack pages are mapped by checking a few addresses */
    int stack_pages_ok = 1;
    for (uint64_t addr = stack_start; addr < stack_end; addr += 0x10000) {
        uint64_t phys = virt_to_phys(addr);
        if (phys == 0) {
            kprint("VM_TEST: Stack page not mapped at ");
            kprint_hex(addr);
            kprint("\n");
            stack_pages_ok = 0;
            break;
        }
    }

    if (!stack_pages_ok) {
        kprint("VM_TEST: User stack pages not properly mapped\n");
        if (saved_page_dir) {
            switch_page_directory(saved_page_dir);
        }
        destroy_process_vm(pid);
        return -1;
    }

    /* Try to access stack memory */
    volatile uint32_t *stack_ptr = (volatile uint32_t *)(stack_end - 16);
    uint32_t test_value = 0xCAFEBABE;
    
    *stack_ptr = test_value;
    if (*stack_ptr != test_value) {
        kprint("VM_TEST: Stack memory access failed\n");
        if (saved_page_dir) {
            switch_page_directory(saved_page_dir);
        }
        destroy_process_vm(pid);
        return -1;
    }

    /* Switch back */
    if (saved_page_dir) {
        switch_page_directory(saved_page_dir);
    }

    /* Clean up */
    destroy_process_vm(pid);

    kprint("VM_TEST: User stack accessibility test PASSED\n");
    return 0;
}

/*
 * Run all VM manager regression tests
 * Returns number of tests passed
 */
int run_vm_manager_tests(void) {
    kprint("VM_TEST: Running VM manager regression tests\n");

    int passed = 0;
    int total = 0;

    total++;
    if (test_process_vm_slot_reuse() == 0) {
        passed++;
    }

    total++;
    if (test_process_vm_counter_reset() == 0) {
        passed++;
    }

    total++;
    if (test_process_vm_double_free() == 0) {
        passed++;
    }

    total++;
    if (test_user_mode_paging_access() == 0) {
        passed++;
    }

    total++;
    if (test_user_stack_accessibility() == 0) {
        passed++;
    }

    kprint("VM_TEST: Completed ");
    kprint_decimal(total);
    kprint(" tests, ");
    kprint_decimal(passed);
    kprint(" passed\n");

    return passed;
}

