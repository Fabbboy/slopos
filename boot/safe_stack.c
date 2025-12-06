/*
 * SlopOS Safe Exception Stack Management
 * Provides dedicated IST stacks with guard pages for critical exceptions
 */

#include "safe_stack.h"
#include "constants.h"
#include "idt.h"
#include "gdt.h"
#include "../lib/klog.h"

#include "../drivers/serial.h"
#include "../mm/page_alloc.h"
#include "../mm/paging.h"
#include "../mm/phys_virt.h"

#include <stdint.h>
#include <stddef.h>

#include "kernel_panic.h"

/* Track diagnostic state for each managed stack */
struct exception_stack_info {
    const char *name;
    uint8_t vector;
    uint8_t ist_index;
    uint64_t region_base;
    uint64_t guard_start;
    uint64_t guard_end;
    uint64_t stack_base;
    uint64_t stack_top;
    uint64_t stack_size;
    uint64_t peak_usage;
    int out_of_bounds_reported;
};

static struct exception_stack_info stack_table[] = {
    {
        .name = "Double Fault",
        .vector = EXCEPTION_DOUBLE_FAULT,
        .ist_index = 1
    },
    {
        .name = "Stack Fault",
        .vector = EXCEPTION_STACK_FAULT,
        .ist_index = 2
    },
    {
        .name = "General Protection",
        .vector = EXCEPTION_GENERAL_PROTECTION,
        .ist_index = 3
    },
    {
        .name = "Page Fault",
        .vector = EXCEPTION_PAGE_FAULT,
        .ist_index = 4
    },
};

static struct exception_stack_info *find_stack_by_vector(uint8_t vector) {
    for (size_t i = 0; i < sizeof(stack_table) / sizeof(stack_table[0]); i++) {
        if (stack_table[i].vector == vector) {
            return &stack_table[i];
        }
    }
    return NULL;
}

static struct exception_stack_info *find_stack_by_address(uint64_t addr) {
    for (size_t i = 0; i < sizeof(stack_table) / sizeof(stack_table[0]); i++) {
        struct exception_stack_info *info = &stack_table[i];
        if (addr >= info->guard_start && addr < info->stack_top) {
            return info;
        }
    }
    return NULL;
}

static void map_stack_pages(struct exception_stack_info *stack) {
    for (uint32_t page = 0; page < EXCEPTION_STACK_PAGES; page++) {
        uint64_t virt_addr = stack->stack_base + ((uint64_t)page * PAGE_SIZE_4KB);
        uint64_t phys_addr = alloc_page_frame(0);
        if (!phys_addr) {
            kernel_panic("safe_stack_init: Failed to allocate exception stack page");
        }

        if (mm_zero_physical_page(phys_addr) != 0) {
            kernel_panic("safe_stack_init: Failed to zero exception stack page");
        }

        if (map_page_4kb(virt_addr, phys_addr, PAGE_KERNEL_RW) != 0) {
            kernel_panic("safe_stack_init: Failed to map exception stack page");
        }
    }
}

void safe_stack_init(void) {
    klog_debug("SAFE STACK: Initializing dedicated IST stacks");

    for (size_t i = 0; i < sizeof(stack_table) / sizeof(stack_table[0]); i++) {
        struct exception_stack_info *stack = &stack_table[i];

        stack->region_base = EXCEPTION_STACK_REGION_BASE +
                             (uint64_t)i * EXCEPTION_STACK_REGION_STRIDE;
        stack->guard_start = stack->region_base;
        stack->guard_end = stack->guard_start + EXCEPTION_STACK_GUARD_SIZE;
        stack->stack_base = stack->guard_end;
        stack->stack_top = stack->stack_base + EXCEPTION_STACK_SIZE;
        stack->stack_size = EXCEPTION_STACK_SIZE;
        stack->peak_usage = 0;
        stack->out_of_bounds_reported = 0;

        map_stack_pages(stack);

        gdt_set_ist(stack->ist_index, stack->stack_top);
        idt_set_ist(stack->vector, stack->ist_index);

        KLOG_BLOCK(KLOG_DEBUG, {
            kprint("SAFE STACK: Vector ");
            kprint_decimal(stack->vector);
            kprint(" uses IST");
            kprint_decimal(stack->ist_index);
            kprint(" @ ");
            kprint_hex(stack->stack_base);
            kprint(" - ");
            kprint_hex(stack->stack_top);
            kprintln("");
        });
    }

    klog_debug("SAFE STACK: IST stacks ready");
}

void safe_stack_record_usage(uint8_t vector, uint64_t frame_ptr) {
    struct exception_stack_info *stack = find_stack_by_vector(vector);
    if (!stack) {
        return;
    }

    if (frame_ptr < stack->stack_base || frame_ptr > stack->stack_top) {
        if (!stack->out_of_bounds_reported) {
            KLOG_BLOCK(KLOG_INFO, {
                kprint("SAFE STACK WARNING: RSP outside managed stack for vector ");
                kprint_decimal(vector);
                kprintln("");
            });
            stack->out_of_bounds_reported = 1;
        }
        return;
    }

    uint64_t usage = stack->stack_top - frame_ptr;
    if (usage > stack->peak_usage) {
        stack->peak_usage = usage;

        KLOG_BLOCK(KLOG_DEBUG, {
            kprint("SAFE STACK: New peak usage on ");
            kprint(stack->name);
            kprint(" stack: ");
            kprint_decimal(usage);
            kprint(" bytes");
            kprintln("");
        });

        if (usage > stack->stack_size - PAGE_SIZE_4KB) {
            KLOG_BLOCK(KLOG_INFO, {
                kprint("SAFE STACK WARNING: ");
                kprint(stack->name);
                kprintln(" stack within one page of guard");
            });
        }
    }
}

int safe_stack_guard_fault(uint64_t fault_addr, const char **stack_name) {
    struct exception_stack_info *stack = find_stack_by_address(fault_addr);
    if (!stack) {
        return 0;
    }

    if (fault_addr >= stack->guard_start && fault_addr < stack->guard_end) {
        if (stack_name) {
            *stack_name = stack->name;
        }
        return 1;
    }

    return 0;
}
