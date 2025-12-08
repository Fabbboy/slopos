/*
 * SlopOS Interrupt Descriptor Table (IDT) Implementation
 * x86_64 IDT setup and exception handling
 */

#include "idt.h"
#include "safe_stack.h"
#include "gdt_defs.h"
#include "../lib/klog.h"
#include "../lib/kdiag.h"
#include "../drivers/serial.h"
#include "../drivers/irq.h"
#include "../drivers/syscall.h"
#include "kernel_panic.h"
#include "../sched/scheduler.h"
#include "../sched/task.h"

// Global IDT and pointer
static struct idt_entry idt[IDT_ENTRIES];
static struct idt_ptr idt_pointer;

static void initialize_handler_tables(void);
static void exception_default_panic(struct interrupt_frame *frame);
static int is_critical_exception_internal(uint8_t vector);

// Exception handler tables
static exception_handler_t panic_handlers[32] = {0};
static exception_handler_t override_handlers[32] = {0};
static enum exception_mode current_exception_mode = EXCEPTION_MODE_NORMAL;

/*
 * Initialize the IDT with default exception handlers
 */
void idt_init(void) {
    klog_debug("IDT: Initializing Interrupt Descriptor Table");

    // Clear the IDT using byte-level access
    // NOTE: Direct struct member access in loops caused page faults due to
    // compiler optimization or alignment issues. Byte-level clearing works reliably.
    volatile uint8_t *idt_bytes = (volatile uint8_t *)&idt;
    for (size_t i = 0; i < sizeof(idt); i++) {
        idt_bytes[i] = 0;
    }

    // Set up the IDT pointer
    idt_pointer.limit = (sizeof(struct idt_entry) * IDT_ENTRIES) - 1;
    idt_pointer.base = (uint64_t)&idt;

    klog_debug("IDT: Set up IDT pointer");

    // Install exception handlers
    // Exceptions 0-19 are defined by Intel
    klog_debug("IDT: Installing exception handlers...");
    idt_set_gate(0, (uint64_t)isr0, 0x08, IDT_GATE_INTERRUPT);   // Divide Error
    idt_set_gate(1, (uint64_t)isr1, 0x08, IDT_GATE_INTERRUPT);   // Debug
    idt_set_gate(2, (uint64_t)isr2, 0x08, IDT_GATE_INTERRUPT);   // NMI
    idt_set_gate(3, (uint64_t)isr3, 0x08, IDT_GATE_TRAP);        // Breakpoint
    idt_set_gate(4, (uint64_t)isr4, 0x08, IDT_GATE_TRAP);        // Overflow
    idt_set_gate(5, (uint64_t)isr5, 0x08, IDT_GATE_INTERRUPT);   // Bound Range
    idt_set_gate(6, (uint64_t)isr6, 0x08, IDT_GATE_INTERRUPT);   // Invalid Opcode
    idt_set_gate(7, (uint64_t)isr7, 0x08, IDT_GATE_INTERRUPT);   // Device Not Available
    idt_set_gate(8, (uint64_t)isr8, 0x08, IDT_GATE_INTERRUPT);   // Double Fault
    // Vector 9 is reserved
    idt_set_gate(10, (uint64_t)isr10, 0x08, IDT_GATE_INTERRUPT); // Invalid TSS
    idt_set_gate(11, (uint64_t)isr11, 0x08, IDT_GATE_INTERRUPT); // Segment Not Present
    idt_set_gate(12, (uint64_t)isr12, 0x08, IDT_GATE_INTERRUPT); // Stack Fault
    idt_set_gate(13, (uint64_t)isr13, 0x08, IDT_GATE_INTERRUPT); // General Protection
    idt_set_gate(14, (uint64_t)isr14, 0x08, IDT_GATE_INTERRUPT); // Page Fault
    // Vector 15 is reserved
    idt_set_gate(16, (uint64_t)isr16, 0x08, IDT_GATE_INTERRUPT); // FPU Error
    idt_set_gate(17, (uint64_t)isr17, 0x08, IDT_GATE_INTERRUPT); // Alignment Check
    idt_set_gate(18, (uint64_t)isr18, 0x08, IDT_GATE_INTERRUPT); // Machine Check
    idt_set_gate(19, (uint64_t)isr19, 0x08, IDT_GATE_INTERRUPT); // SIMD FP Exception

    // Install IRQ handlers (vectors 32-47)
    idt_set_gate(32, (uint64_t)irq0, 0x08, IDT_GATE_INTERRUPT);  // Timer
    idt_set_gate(33, (uint64_t)irq1, 0x08, IDT_GATE_INTERRUPT);  // Keyboard
    idt_set_gate(34, (uint64_t)irq2, 0x08, IDT_GATE_INTERRUPT);  // Cascade
    idt_set_gate(35, (uint64_t)irq3, 0x08, IDT_GATE_INTERRUPT);  // COM2
    idt_set_gate(36, (uint64_t)irq4, 0x08, IDT_GATE_INTERRUPT);  // COM1
    idt_set_gate(37, (uint64_t)irq5, 0x08, IDT_GATE_INTERRUPT);  // LPT2
    idt_set_gate(38, (uint64_t)irq6, 0x08, IDT_GATE_INTERRUPT);  // Floppy
    idt_set_gate(39, (uint64_t)irq7, 0x08, IDT_GATE_INTERRUPT);  // LPT1
    idt_set_gate(40, (uint64_t)irq8, 0x08, IDT_GATE_INTERRUPT);  // RTC
    idt_set_gate(41, (uint64_t)irq9, 0x08, IDT_GATE_INTERRUPT);  // Free
    idt_set_gate(42, (uint64_t)irq10, 0x08, IDT_GATE_INTERRUPT); // Free
    idt_set_gate(43, (uint64_t)irq11, 0x08, IDT_GATE_INTERRUPT); // Free
    idt_set_gate(44, (uint64_t)irq12, 0x08, IDT_GATE_INTERRUPT); // Mouse
    idt_set_gate(45, (uint64_t)irq13, 0x08, IDT_GATE_INTERRUPT); // FPU
    idt_set_gate(46, (uint64_t)irq14, 0x08, IDT_GATE_INTERRUPT); // ATA Primary
    idt_set_gate(47, (uint64_t)irq15, 0x08, IDT_GATE_INTERRUPT); // ATA Secondary

    /* User-accessible syscall gate (int 0x80) */
    idt_set_gate_priv(SYSCALL_VECTOR, (uint64_t)isr128, GDT_CODE_SELECTOR, IDT_GATE_TRAP, 3);

    initialize_handler_tables();

    klog_printf(KLOG_DEBUG, "IDT: Configured %u interrupt vectors\n", IDT_ENTRIES);
}

/*
 * Set an IDT gate
 */
void idt_set_gate_priv(uint8_t vector, uint64_t handler, uint16_t selector, uint8_t type, uint8_t dpl) {
    idt[vector].offset_low = handler & 0xFFFF;
    idt[vector].selector = selector;
    idt[vector].ist = 0;  // No separate interrupt stacks for now
    idt[vector].type_attr = type | 0x80 | ((dpl & 0x3) << 5);  // Present=1, configurable DPL
    idt[vector].offset_mid = (handler >> 16) & 0xFFFF;
    idt[vector].offset_high = (handler >> 32) & 0xFFFFFFFF;
    idt[vector].zero = 0;
}

void idt_set_gate(uint8_t vector, uint64_t handler, uint16_t selector, uint8_t type) {
    idt_set_gate_priv(vector, handler, selector, type, 0);
}

int idt_get_gate(uint8_t vector, struct idt_entry *out_entry) {
    if (!out_entry) {
        return -1;
    }
    if ((unsigned int)vector >= IDT_ENTRIES) {
        return -1;
    }
    *out_entry = idt[vector];
    return 0;
}

void idt_set_ist(uint8_t vector, uint8_t ist_index) {
    if ((uint16_t)vector >= IDT_ENTRIES) {
        klog_printf(KLOG_INFO, "IDT: Invalid IST assignment for vector %u\n", vector);
        return;
    }

    if (ist_index > 7) {
        klog_printf(KLOG_INFO, "IDT: Invalid IST index %u\n", ist_index);
        return;
    }

    idt[vector].ist = ist_index & 0x7;
}

/*
 * Install a custom exception handler
 */
void idt_install_exception_handler(uint8_t vector, exception_handler_t handler) {
    if (vector >= 32) {
        klog_printf(KLOG_INFO, "IDT: Ignoring handler install for non-exception vector %u\n", vector);
        return;
    }

    if (handler != NULL && is_critical_exception_internal(vector)) {
        klog_printf(KLOG_INFO, "IDT: Refusing to override critical exception %u\n", vector);
        return;
    }

    if (override_handlers[vector] == handler) {
        return;
    }

    override_handlers[vector] = handler;

    if (handler != NULL) {
        klog_printf(KLOG_DEBUG, "IDT: Registered override handler for exception %u\n", vector);
    } else {
        klog_printf(KLOG_DEBUG, "IDT: Cleared override handler for exception %u\n", vector);
    }
}

static void initialize_handler_tables(void) {
    for (int i = 0; i < 32; i++) {
        panic_handlers[i] = exception_default_panic;
        override_handlers[i] = NULL;
    }

    panic_handlers[EXCEPTION_DIVIDE_ERROR] = exception_divide_error;
    panic_handlers[EXCEPTION_DEBUG] = exception_debug;
    panic_handlers[EXCEPTION_NMI] = exception_nmi;
    panic_handlers[EXCEPTION_BREAKPOINT] = exception_breakpoint;
    panic_handlers[EXCEPTION_OVERFLOW] = exception_overflow;
    panic_handlers[EXCEPTION_BOUND_RANGE] = exception_bound_range;
    panic_handlers[EXCEPTION_INVALID_OPCODE] = exception_invalid_opcode;
    panic_handlers[EXCEPTION_DEVICE_NOT_AVAIL] = exception_device_not_available;
    panic_handlers[EXCEPTION_DOUBLE_FAULT] = exception_double_fault;
    panic_handlers[EXCEPTION_INVALID_TSS] = exception_invalid_tss;
    panic_handlers[EXCEPTION_SEGMENT_NOT_PRES] = exception_segment_not_present;
    panic_handlers[EXCEPTION_STACK_FAULT] = exception_stack_fault;
    panic_handlers[EXCEPTION_GENERAL_PROTECTION] = exception_general_protection;
    panic_handlers[EXCEPTION_PAGE_FAULT] = exception_page_fault;
    panic_handlers[EXCEPTION_FPU_ERROR] = exception_fpu_error;
    panic_handlers[EXCEPTION_ALIGNMENT_CHECK] = exception_alignment_check;
    panic_handlers[EXCEPTION_MACHINE_CHECK] = exception_machine_check;
    panic_handlers[EXCEPTION_SIMD_FP_EXCEPTION] = exception_simd_fp_exception;
}

static int is_critical_exception_internal(uint8_t vector) {
    return vector == EXCEPTION_DOUBLE_FAULT ||
           vector == EXCEPTION_MACHINE_CHECK ||
           vector == EXCEPTION_NMI;
}

void exception_set_mode(enum exception_mode mode) {
    current_exception_mode = mode;

    if (mode == EXCEPTION_MODE_NORMAL) {
        for (int i = 0; i < 32; i++) {
            override_handlers[i] = NULL;
        }
    }
}

int exception_is_critical(uint8_t vector) {
    return is_critical_exception_internal(vector);
}

/*
 * Load the IDT
 */
void idt_load(void) {
    klog_printf(KLOG_DEBUG, "IDT: Loading IDT at address 0x%llx with limit 0x%llx\n",
                (unsigned long long)idt_pointer.base,
                (unsigned long long)idt_pointer.limit);

    // Load the IDT using assembly
    __asm__ volatile ("lidt %0" : : "m" (idt_pointer));

    klog_debug("IDT: Successfully loaded");
}

/*
 * Common exception handler dispatcher
 */
void common_exception_handler(struct interrupt_frame *frame) {
    uint8_t vector = (uint8_t)(frame->vector & 0xFF);

    safe_stack_record_usage(vector, (uint64_t)frame);

    if (vector == SYSCALL_VECTOR) {
        syscall_handle(frame);
        return;
    }

    if (vector >= IRQ_BASE_VECTOR) {
        irq_dispatch(frame);
        return;
    }

    if (vector >= 32) {
        klog_printf(KLOG_INFO, "EXCEPTION: Unknown vector %u\n", vector);
        exception_default_panic(frame);
        return;
    }

    int critical = is_critical_exception_internal(vector);
    if (critical || current_exception_mode != EXCEPTION_MODE_TEST) {
        klog_printf(KLOG_INFO, "EXCEPTION: Vector %u (%s)\n",
                    vector, get_exception_name(vector));
    }

    exception_handler_t handler = panic_handlers[vector];

    if (!critical && current_exception_mode == EXCEPTION_MODE_TEST &&
        override_handlers[vector] != NULL) {
        handler = override_handlers[vector];
    }

    if (handler == NULL) {
        handler = exception_default_panic;
    }

    handler(frame);
}

/*
 * Get exception name string
 */
const char *get_exception_name(uint8_t vector) {
    static const char *exception_names[] = {
        "Divide Error",
        "Debug",
        "Non-Maskable Interrupt",
        "Breakpoint",
        "Overflow",
        "Bound Range Exceeded",
        "Invalid Opcode",
        "Device Not Available",
        "Double Fault",
        "Coprocessor Segment Overrun",
        "Invalid TSS",
        "Segment Not Present",
        "Stack Segment Fault",
        "General Protection Fault",
        "Page Fault",
        "Reserved",
        "x87 FPU Error",
        "Alignment Check",
        "Machine Check",
        "SIMD Floating-Point Exception"
    };

    if (vector < 20) {
        return exception_names[vector];
    } else if (vector >= 32 && vector < 48) {
        return "Hardware IRQ";
    } else {
        return "Unknown";
    }
}

static void exception_default_panic(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "FATAL: Unhandled exception\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Unhandled exception");
}

/*
 * Default exception handlers
 */

void exception_divide_error(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "FATAL: Divide by zero error\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Divide by zero error");
}

void exception_debug(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "DEBUG: Debug exception occurred\n");
    kdiag_dump_interrupt_frame(frame);
}

void exception_nmi(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "FATAL: Non-maskable interrupt\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Non-maskable interrupt");
}

void exception_breakpoint(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "DEBUG: Breakpoint exception\n");
    kdiag_dump_interrupt_frame(frame);
}

void exception_overflow(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "ERROR: Overflow exception\n");
    kdiag_dump_interrupt_frame(frame);
}

void exception_bound_range(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "ERROR: Bound range exceeded\n");
    kdiag_dump_interrupt_frame(frame);
}

enum user_fault_reason {
    USER_FAULT_PAGE = 0,
    USER_FAULT_GP,
    USER_FAULT_UD,
    USER_FAULT_DEVICE_NA,
    USER_FAULT_MAX
};

static const char *user_fault_reason_str[USER_FAULT_MAX] = {
    [USER_FAULT_PAGE] = "user page fault",
    [USER_FAULT_GP] = "user general protection fault (likely privileged instruction or bad segment)",
    [USER_FAULT_UD] = "user invalid opcode",
    [USER_FAULT_DEVICE_NA] = "user device not available",
};

static int in_user(const struct interrupt_frame *frame) {
    return (frame->cs & 0x3) == 0x3;
}

static void terminate_user_task(enum user_fault_reason reason,
                                struct interrupt_frame *frame,
                                const char *detail) {
    task_t *task = scheduler_get_current_task();
    uint32_t tid = task ? task->task_id : INVALID_TASK_ID;
    const char *why = (reason < USER_FAULT_MAX) ? user_fault_reason_str[reason] : "user fault";

    klog_printf(KLOG_INFO, "Terminating user task %u: %s\n", tid, why);
    if (detail) {
        klog_printf(KLOG_INFO, "Detail: %s\n", detail);
    }
    if (task) {
        task_terminate(tid);
        scheduler_request_reschedule_from_interrupt();
    }
    (void)frame;
}

void exception_invalid_opcode(struct interrupt_frame *frame) {
    if (in_user(frame)) {
        terminate_user_task(USER_FAULT_UD, frame, "invalid opcode in user mode");
        return;
    }
    klog_printf(KLOG_INFO, "FATAL: Invalid opcode\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Invalid opcode");
}

void exception_device_not_available(struct interrupt_frame *frame) {
    if (in_user(frame)) {
        terminate_user_task(USER_FAULT_DEVICE_NA, frame, "device not available in user mode");
        return;
    }
    klog_printf(KLOG_INFO, "ERROR: Device not available\n");
    kdiag_dump_interrupt_frame(frame);
}

void exception_double_fault(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "FATAL: Double fault\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Double fault");
}

void exception_invalid_tss(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "FATAL: Invalid TSS\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Invalid TSS");
}

void exception_segment_not_present(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "FATAL: Segment not present\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Segment not present");
}

void exception_stack_fault(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "FATAL: Stack segment fault\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Stack segment fault");
}

void exception_general_protection(struct interrupt_frame *frame) {
    if (in_user(frame)) {
        terminate_user_task(USER_FAULT_GP, frame, "general protection from user mode");
        return;
    }
    klog_printf(KLOG_INFO, "FATAL: General protection fault\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("General protection fault");
}

void exception_page_fault(struct interrupt_frame *frame) {
    uint64_t fault_addr;
    __asm__ volatile ("movq %%cr2, %0" : "=r" (fault_addr));

    const char *stack_name = NULL;
    if (safe_stack_guard_fault(fault_addr, &stack_name)) {
        klog_printf(KLOG_INFO, "FATAL: Exception stack overflow detected via guard page\n");
        if (stack_name) {
            klog_printf(KLOG_INFO, "Guard page owner: %s\n", stack_name);
        }
        klog_printf(KLOG_INFO, "Fault address: 0x%lx\n", fault_addr);

        kdiag_dump_interrupt_frame(frame);
        kernel_panic("Exception stack overflow");
        return;
    }

    int from_user = in_user(frame);

    klog_printf(KLOG_INFO, "FATAL: Page fault\n");
    klog_printf(KLOG_INFO, "Fault address: 0x%lx\n", fault_addr);

    klog_printf(KLOG_INFO, "Error code: 0x%lx (%s) (%s) (%s)\n",
                frame->error_code,
                (frame->error_code & 1) ? "Page present" : "Page not present",
                (frame->error_code & 2) ? "Write" : "Read",
                (frame->error_code & 4) ? "User" : "Supervisor");

    if (from_user) {
        terminate_user_task(USER_FAULT_PAGE, frame, "user page fault");
        return;
    }

    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Page fault");
}

void exception_fpu_error(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "ERROR: x87 FPU error\n");
    kdiag_dump_interrupt_frame(frame);
}

void exception_alignment_check(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "ERROR: Alignment check\n");
    kdiag_dump_interrupt_frame(frame);
}

void exception_machine_check(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "FATAL: Machine check\n");
    kdiag_dump_interrupt_frame(frame);
    kernel_panic("Machine check");
}

void exception_simd_fp_exception(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "ERROR: SIMD floating-point exception\n");
    kdiag_dump_interrupt_frame(frame);
}
