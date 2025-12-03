/*
 * SlopOS Debug Utilities
 * Enhanced debugging and diagnostic functions
 */

#include "debug.h"
#include "log.h"
#include "../drivers/serial.h"
#include "../lib/cpu.h"
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

// Simple symbol table (for kernel symbols)
#define MAX_SYMBOLS 256
static struct {
    char name[32];
    uint64_t address;
} symbol_table[MAX_SYMBOLS];
static int symbol_count = 0;

// Memory regions
#define MAX_MEMORY_REGIONS 64
static struct memory_region memory_regions[MAX_MEMORY_REGIONS];
static int memory_region_count = 0;

static inline uint64_t debug_read_tsc(void) {
    return cpu_read_tsc();
}

/*
 * Initialize debug subsystem
 */
void debug_init(void) {
    boot_log_debug("DEBUG: Initializing debug subsystem");

    debug_ctx.boot_timestamp = debug_get_timestamp();
    debug_ctx.initialized = 1;

    // Register basic kernel memory regions
    debug_register_memory_region(0xFFFFFFFF80000000ULL, 0xFFFFFFFF80400000ULL, 0, "Kernel Code");
    debug_register_memory_region(0x0000000000000000ULL, 0x0000000000100000ULL, 0, "Low Memory");

    boot_log_debug("DEBUG: Debug subsystem initialized");
}

/*
 * Set debug level
 */
void debug_set_level(int level) {
    debug_ctx.debug_level = level;
    kprint("DEBUG: Set debug level to ");
    kprint_dec(level);
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

    uint64_t tsc = debug_read_tsc();
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
        kprint_dec(line);
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
    __asm__ volatile ("movq %%rbp, %0" : "=r" (rbp));
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
    kprint_dec(frame->vector);
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
    uint64_t rbp;
    __asm__ volatile ("movq %%rbp, %0" : "=r" (rbp));

    kprintln("=== STACK TRACE ===");
    debug_dump_stack_trace_from_rbp(rbp);
    kprintln("=== END STACK TRACE ===");
}

/*
 * Walk stack from given RBP
 */
void debug_dump_stack_trace_from_rbp(uint64_t rbp) {
    int frame_count = 0;
    uint64_t current_rbp = rbp;

    while (current_rbp != 0 && frame_count < STACK_TRACE_DEPTH) {
        // Check if RBP looks valid
        if (!debug_is_valid_memory_address(current_rbp) ||
            !debug_is_valid_memory_address(current_rbp + 8)) {
            kprint("Frame ");
            kprint_dec(frame_count);
            kprint(": Invalid RBP ");
            kprint_hex(current_rbp);
            kprintln("");
            break;
        }

        // Read return address and previous RBP
        uint64_t *stack_ptr = (uint64_t *)current_rbp;
        uint64_t prev_rbp = stack_ptr[0];
        uint64_t return_addr = stack_ptr[1];

        kprint("Frame ");
        kprint_dec(frame_count);
        kprint(": RBP=");
        kprint_hex(current_rbp);
        kprint(" RIP=");
        kprint_hex(return_addr);

        // Try to resolve symbol
        const char *symbol = debug_get_symbol_name(return_addr);
        if (symbol) {
            kprint(" (");
            kprint(symbol);
            kprint(")");
        }
        kprintln("");

        current_rbp = prev_rbp;
        frame_count++;

        // Sanity check to prevent infinite loops
        if (current_rbp <= rbp) {
            kprintln("Frame: Stack frame loop detected, stopping");
            break;
        }
    }

    if (frame_count == 0) {
        kprintln("No stack frames found");
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
 * Check if memory address appears valid
 */
int debug_is_valid_memory_address(uint64_t address) {
    // Basic checks for obviously invalid addresses
    if (address == 0) return 0;
    if (address < 0x1000) return 0;  // Null pointer area
    if (address >= 0xFFFF800000000000ULL && address < 0xFFFFFFFF80000000ULL) return 0;  // Non-canonical

    // For now, assume kernel addresses are valid
    if (address >= 0xFFFFFFFF80000000ULL) return 1;

    // Could add more sophisticated checks here
    return 1;
}

/*
 * Dump memory around address
 */
void debug_dump_memory(uint64_t address, size_t length) {
    if (!debug_is_valid_memory_address(address)) {
        kprint("Invalid memory address: ");
        kprint_hex(address);
        kprintln("");
        return;
    }

    kprint("Memory dump at ");
    kprint_hex(address);
    kprint(" (");
    kprint_dec(length);
    kprintln(" bytes):");

    debug_hexdump((void *)address, length, address);
}

/*
 * Dump memory around RIP
 */
void debug_dump_memory_around_rip(uint64_t rip) {
    kprintln("Code around RIP:");
    debug_dump_memory(rip - 32, 64);
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
                kprint_char(c);
            } else {
                kprint_char('.');
            }
        }

        kprintln("|");
    }
}

/*
 * Analyze exception
 */
void debug_analyze_exception(struct interrupt_frame *frame) {
    kprintln("=== EXCEPTION ANALYSIS ===");

    switch (frame->vector) {
        case EXCEPTION_PAGE_FAULT:
            debug_analyze_page_fault(frame);
            break;
        case EXCEPTION_GENERAL_PROTECTION:
            debug_analyze_general_protection(frame);
            break;
        case EXCEPTION_DOUBLE_FAULT:
            debug_analyze_double_fault(frame);
            break;
        default:
            kprint("Exception ");
            kprint_dec(frame->vector);
            kprint(" (");
            kprint(get_exception_name(frame->vector));
            kprintln(") - no specific analysis available");
            break;
    }

    kprintln("=== END EXCEPTION ANALYSIS ===");
}

/*
 * Analyze page fault
 */
void debug_analyze_page_fault(struct interrupt_frame *frame) {
    uint64_t fault_addr;
    __asm__ volatile ("movq %%cr2, %0" : "=r" (fault_addr));

    kprintln("PAGE FAULT ANALYSIS:");
    kprint("Fault address: ");
    kprint_hex(fault_addr);
    kprintln("");

    kprint("Error code: ");
    kprint_hex(frame->error_code);
    kprint(" (");
    if (frame->error_code & 1) kprint("Protection violation");
    else kprint("Page not present");
    if (frame->error_code & 2) kprint(", Write");
    else kprint(", Read");
    if (frame->error_code & 4) kprint(", User mode");
    else kprint(", Supervisor mode");
    if (frame->error_code & 8) kprint(", Reserved bit violation");
    if (frame->error_code & 16) kprint(", Instruction fetch");
    kprintln(")");

    // Find memory region
    struct memory_region *region = debug_find_memory_region(fault_addr);
    if (region) {
        kprint("Memory region: ");
        kprint(region->name);
        kprintln("");
    } else {
        kprintln("Memory region: Unknown/Unmapped");
    }
}

/*
 * Simple symbol resolution
 */
const char *debug_get_symbol_name(uint64_t address) {
    for (int i = 0; i < symbol_count; i++) {
        if (symbol_table[i].address == address) {
            return symbol_table[i].name;
        }
    }
    return NULL;
}

/*
 * Add symbol to table
 */
int debug_add_symbol(const char *name, uint64_t address) {
    if (symbol_count >= MAX_SYMBOLS) return -1;

    int i = 0;
    while (name && name[i] && i < 31) {
        symbol_table[symbol_count].name[i] = name[i];
        i++;
    }
    symbol_table[symbol_count].name[i] = '\0';
    symbol_table[symbol_count].address = address;
    symbol_count++;

    return 0;
}

/*
 * Register memory region
 */
void debug_register_memory_region(uint64_t start, uint64_t end, uint32_t flags, const char *name) {
    if (memory_region_count >= MAX_MEMORY_REGIONS) return;

    memory_regions[memory_region_count].start = start;
    memory_regions[memory_region_count].end = end;
    memory_regions[memory_region_count].flags = flags;

    int i = 0;
    while (name && name[i] && i < 31) {
        memory_regions[memory_region_count].name[i] = name[i];
        i++;
    }
    memory_regions[memory_region_count].name[i] = '\0';

    memory_region_count++;
}

/*
 * Find memory region containing address
 */
struct memory_region *debug_find_memory_region(uint64_t address) {
    for (int i = 0; i < memory_region_count; i++) {
        if (address >= memory_regions[i].start && address < memory_regions[i].end) {
            return &memory_regions[i];
        }
    }
    return NULL;
}

/*
 * Analyze general protection fault
 */
void debug_analyze_general_protection(struct interrupt_frame *frame) {
    kprintln("=== GENERAL PROTECTION FAULT (#GP) ===");
    kprint("Error Code: ");
    kprint_hex(frame->error_code);
    kprintln("");
    
    // Decode error code
    if (frame->error_code & 0x01) {
        kprintln("External event caused exception");
    }
    
    uint16_t selector_index = (frame->error_code >> 3) & 0x1FFF;
    uint8_t table = (frame->error_code >> 1) & 0x03;
    
    kprint("Selector Index: ");
    kprint_hex(selector_index);
    kprint(" Table: ");
    if (table == 0) kprintln("GDT");
    else if (table == 1) kprintln("IDT");
    else if (table == 2) kprintln("LDT");
    else kprintln("Unknown");
    
    kprint("RIP: ");
    kprint_hex(frame->rip);
    kprintln("");
}

/*
 * Analyze double fault
 */
void debug_analyze_double_fault(struct interrupt_frame *frame) {
    kprintln("=== DOUBLE FAULT (#DF) ===");
    kprintln("CRITICAL: A double fault indicates a severe kernel error");
    kprintln("This usually means an exception occurred while handling another exception");
    
    kprint("Error Code: ");
    kprint_hex(frame->error_code);
    kprintln(" (always 0 for double fault)");
    
    kprint("RIP: ");
    kprint_hex(frame->rip);
    kprintln("");
    
    kprint("RSP: ");
    kprint_hex(frame->rsp);
    kprintln("");
    
    kprint("CS: ");
    kprint_hex(frame->cs);
    kprintln("");
    
    kprintln("System is likely in an unstable state");
}
