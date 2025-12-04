/*
 * SlopOS Debug Utilities
 * Enhanced debugging and diagnostic functions
 */

#include "debug.h"
#include "constants.h"
#include "log.h"
#include "../drivers/serial.h"
#include "../lib/cpu.h"
#include "../lib/stacktrace.h"
#include "../drivers/irq.h"
#include "idt.h"
#include <stddef.h>

// Global debug context
static struct debug_context debug_ctx = {
    .debug_level = DEBUG_LEVEL_INFO,
    .debug_flags = DEBUG_FLAG_TIMESTAMP,
    .boot_timestamp = 0,
    .initialized = 0
};

/*
 * Initialize debug subsystem
 */
void debug_init(void) {
    boot_log_debug("DEBUG: Initializing debug subsystem");

    debug_ctx.boot_timestamp = debug_get_timestamp();
    debug_ctx.initialized = 1;

    boot_log_debug("DEBUG: Debug subsystem initialized");
}

/*
 * Set debug level
 */
void debug_set_level(int level) {
    debug_ctx.debug_level = level;
    kprint("DEBUG: Set debug level to ");
    kprint_decimal(level);
    kprintln("");
}

/*
 * Set debug flags
 */
void debug_set_flags(uint32_t flags) {
    debug_ctx.debug_flags = flags;
    kprint("DEBUG: Set debug flags to ");
    kprint_hex(flags);
    kprintln("");
}

/*
 * Get debug level
 */
int debug_get_level(void) {
    return debug_ctx.debug_level;
}

/*
 * Get debug flags
 */
uint32_t debug_get_flags(void) {
    return debug_ctx.debug_flags;
}

/*
 * Get current timestamp using timer ticks with TSC fallback
 */
uint64_t debug_get_timestamp(void) {
    static uint64_t monotonic_time = 0;
    static uint64_t last_tick_count = 0;

    uint64_t tick_count = irq_get_timer_ticks();
    if (tick_count > last_tick_count) {
        monotonic_time += tick_count - last_tick_count;
        last_tick_count = tick_count;
    }

    if (tick_count != 0) {
        return monotonic_time;
    }

    uint64_t tsc = cpu_read_tsc();
    if (tsc <= monotonic_time) {
        tsc = monotonic_time + 1;
    }

    monotonic_time = tsc;
    return monotonic_time;
}

/*
 * Print timestamp
 */
void debug_print_timestamp(void) {
    uint64_t ts = debug_get_timestamp() - debug_ctx.boot_timestamp;
    kprint("[+");
    kprint_decimal(ts);
    kprint(" ticks] ");
}

/*
 * Ensure all buffered debug output reaches the serial line
 */
void debug_flush(void) {
    uint16_t port = serial_get_kernel_output();
    serial_flush(port);
}

/*
 * Print location information
 */
void debug_print_location(const char *file, int line, const char *function) {
    kprint("at ");
    if (function) {
        kprint(function);
        kprint("() ");
    }
    if (file) {
        kprint(file);
        kprint(":");
        kprint_decimal(line);
    }
    kprintln("");
}

/*
 * Enhanced CPU state dump
 */
void debug_dump_cpu_state(void) {
    kprintln("=== ENHANCED CPU STATE DUMP ===");

    // Get current register values
    uint64_t rsp, rbp, rax, rbx, rcx, rdx, rsi, rdi;
    uint64_t r8, r9, r10, r11, r12, r13, r14, r15;
    uint64_t rflags, cr0, cr2, cr3, cr4;
    uint16_t cs, ds, es, fs, gs, ss;

    // General purpose registers
    __asm__ volatile ("movq %%rsp, %0" : "=r" (rsp));
    rbp = cpu_read_rbp();
    __asm__ volatile ("movq %%rax, %0" : "=r" (rax));
    __asm__ volatile ("movq %%rbx, %0" : "=r" (rbx));
    __asm__ volatile ("movq %%rcx, %0" : "=r" (rcx));
    __asm__ volatile ("movq %%rdx, %0" : "=r" (rdx));
    __asm__ volatile ("movq %%rsi, %0" : "=r" (rsi));
    __asm__ volatile ("movq %%rdi, %0" : "=r" (rdi));
    __asm__ volatile ("movq %%r8, %0" : "=r" (r8));
    __asm__ volatile ("movq %%r9, %0" : "=r" (r9));
    __asm__ volatile ("movq %%r10, %0" : "=r" (r10));
    __asm__ volatile ("movq %%r11, %0" : "=r" (r11));
    __asm__ volatile ("movq %%r12, %0" : "=r" (r12));
    __asm__ volatile ("movq %%r13, %0" : "=r" (r13));
    __asm__ volatile ("movq %%r14, %0" : "=r" (r14));
    __asm__ volatile ("movq %%r15, %0" : "=r" (r15));

    // Flags
    __asm__ volatile ("pushfq; popq %0" : "=r" (rflags));

    // Segment registers
    __asm__ volatile ("movw %%cs, %0" : "=r" (cs));
    __asm__ volatile ("movw %%ds, %0" : "=r" (ds));
    __asm__ volatile ("movw %%es, %0" : "=r" (es));
    __asm__ volatile ("movw %%fs, %0" : "=r" (fs));
    __asm__ volatile ("movw %%gs, %0" : "=r" (gs));
    __asm__ volatile ("movw %%ss, %0" : "=r" (ss));

    // Control registers
    __asm__ volatile ("movq %%cr0, %0" : "=r" (cr0));
    __asm__ volatile ("movq %%cr2, %0" : "=r" (cr2));
    __asm__ volatile ("movq %%cr3, %0" : "=r" (cr3));
    __asm__ volatile ("movq %%cr4, %0" : "=r" (cr4));

    // Print general purpose registers in groups
    kprintln("General Purpose Registers:");
    kprint("  RAX: ");
    kprint_hex(rax);
    kprint("  RBX: ");
    kprint_hex(rbx);
    kprint("  RCX: ");
    kprint_hex(rcx);
    kprint("  RDX: ");
    kprint_hex(rdx);
    kprintln("");

    kprint("  RSI: ");
    kprint_hex(rsi);
    kprint("  RDI: ");
    kprint_hex(rdi);
    kprint("  RBP: ");
    kprint_hex(rbp);
    kprint("  RSP: ");
    kprint_hex(rsp);
    kprintln("");

    kprint("  R8:  ");
    kprint_hex(r8);
    kprint("  R9:  ");
    kprint_hex(r9);
    kprint("  R10: ");
    kprint_hex(r10);
    kprint("  R11: ");
    kprint_hex(r11);
    kprintln("");

    kprint("  R12: ");
    kprint_hex(r12);
    kprint("  R13: ");
    kprint_hex(r13);
    kprint("  R14: ");
    kprint_hex(r14);
    kprint("  R15: ");
    kprint_hex(r15);
    kprintln("");

    // Print flags with interpretation
    kprintln("Flags Register:");
    kprint("  RFLAGS: ");
    kprint_hex(rflags);
    kprint(" [");
    if (rflags & (1 << 0)) kprint("CF ");
    if (rflags & (1 << 2)) kprint("PF ");
    if (rflags & (1 << 4)) kprint("AF ");
    if (rflags & (1 << 6)) kprint("ZF ");
    if (rflags & (1 << 7)) kprint("SF ");
    if (rflags & (1 << 8)) kprint("TF ");
    if (rflags & (1 << 9)) kprint("IF ");
    if (rflags & (1 << 10)) kprint("DF ");
    if (rflags & (1 << 11)) kprint("OF ");
    kprintln("]");

    // Print segment registers
    kprintln("Segment Registers:");
    kprint("  CS: ");
    kprint_hex(cs);
    kprint("  DS: ");
    kprint_hex(ds);
    kprint("  ES: ");
    kprint_hex(es);
    kprint("  FS: ");
    kprint_hex(fs);
    kprint("  GS: ");
    kprint_hex(gs);
    kprint("  SS: ");
    kprint_hex(ss);
    kprintln("");

    // Print control registers
    kprintln("Control Registers:");
    kprint("  CR0: ");
    kprint_hex(cr0);
    kprint("  CR2: ");
    kprint_hex(cr2);
    kprintln("");
    kprint("  CR3: ");
    kprint_hex(cr3);
    kprint("  CR4: ");
    kprint_hex(cr4);
    kprintln("");

    kprintln("=== END CPU STATE DUMP ===");
}

/*
 * Dump registers from interrupt frame
 */
void debug_dump_registers_from_frame(struct interrupt_frame *frame) {
    kprintln("=== INTERRUPT FRAME REGISTERS ===");

    kprint("Vector: ");
    kprint_decimal(frame->vector);
    kprint(" (");
    kprint(get_exception_name(frame->vector));
    kprint(")  Error Code: ");
    kprint_hex(frame->error_code);
    kprintln("");

    kprint("RIP: ");
    kprint_hex(frame->rip);
    kprint("  CS: ");
    kprint_hex(frame->cs);
    kprint("  RFLAGS: ");
    kprint_hex(frame->rflags);
    kprintln("");

    kprint("RSP: ");
    kprint_hex(frame->rsp);
    kprint("  SS: ");
    kprint_hex(frame->ss);
    kprintln("");

    kprintln("General Purpose Registers:");
    kprint("  RAX: ");
    kprint_hex(frame->rax);
    kprint("  RBX: ");
    kprint_hex(frame->rbx);
    kprint("  RCX: ");
    kprint_hex(frame->rcx);
    kprint("  RDX: ");
    kprint_hex(frame->rdx);
    kprintln("");

    kprint("  RSI: ");
    kprint_hex(frame->rsi);
    kprint("  RDI: ");
    kprint_hex(frame->rdi);
    kprint("  RBP: ");
    kprint_hex(frame->rbp);
    kprintln("");

    kprint("  R8:  ");
    kprint_hex(frame->r8);
    kprint("  R9:  ");
    kprint_hex(frame->r9);
    kprint("  R10: ");
    kprint_hex(frame->r10);
    kprint("  R11: ");
    kprint_hex(frame->r11);
    kprintln("");

    kprint("  R12: ");
    kprint_hex(frame->r12);
    kprint("  R13: ");
    kprint_hex(frame->r13);
    kprint("  R14: ");
    kprint_hex(frame->r14);
    kprint("  R15: ");
    kprint_hex(frame->r15);
    kprintln("");

    kprintln("=== END INTERRUPT FRAME REGISTERS ===");
}

/*
 * Dump stack trace
 */
void debug_dump_stack_trace(void) {
    uint64_t rbp = cpu_read_rbp();
    kprintln("=== STACK TRACE ===");
    debug_dump_stack_trace_from_rbp(rbp);
    kprintln("=== END STACK TRACE ===");
}

/*
 * Walk stack from given RBP and print each frame
 */
void debug_dump_stack_trace_from_rbp(uint64_t rbp) {
    struct stacktrace_entry entries[STACK_TRACE_DEPTH];
    int frame_count = stacktrace_capture_from(rbp, entries, STACK_TRACE_DEPTH);

    if (frame_count == 0) {
        kprintln("No stack frames found");
        return;
    }

    for (int i = 0; i < frame_count; i++) {
        kprint("Frame ");
        kprint_decimal(i);
        kprint(": RBP=");
        kprint_hex(entries[i].frame_pointer);
        kprint(" RIP=");
        kprint_hex(entries[i].return_address);
        kprintln("");
    }
}

/*
 * Dump stack trace from interrupt frame
 */
void debug_dump_stack_trace_from_frame(struct interrupt_frame *frame) {
    kprintln("=== STACK TRACE FROM EXCEPTION ===");
    kprint("Exception occurred at RIP: ");
    kprint_hex(frame->rip);
    kprintln("");

    debug_dump_stack_trace_from_rbp(frame->rbp);
    kprintln("=== END STACK TRACE ===");
}

/*
 * Hexdump utility
 */
void debug_hexdump(const void *data, size_t length, uint64_t base_address) {
    const uint8_t *bytes = (const uint8_t *)data;
    size_t i, j;

    for (i = 0; i < length; i += 16) {
        // Print address
        kprint_hex(base_address + i);
        kprint(": ");

        // Print hex bytes
        for (j = 0; j < 16 && i + j < length; j++) {
            if (j == 8) kprint(" ");  // Extra space in middle
            kprint_hex_byte(bytes[i + j]);
            kprint(" ");
        }

        // Pad if short line
        for (; j < 16; j++) {
            if (j == 8) kprint(" ");
            kprint("   ");
        }

        kprint(" |");

        // Print ASCII representation
        for (j = 0; j < 16 && i + j < length; j++) {
            uint8_t c = bytes[i + j];
            if (c >= 32 && c <= 126) {
                serial_putc(serial_get_kernel_output(), c);
            } else {
                serial_putc(serial_get_kernel_output(), '.');
            }
        }

        kprintln("|");
    }
}

