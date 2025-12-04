/*
 * SlopOS Debug Utilities
 * Enhanced debugging and diagnostic functions
 */

#ifndef DEBUG_H
#define DEBUG_H

#include <stdint.h>
#include <stddef.h>
#include "idt.h"

// Debug levels
#define DEBUG_LEVEL_NONE    0
#define DEBUG_LEVEL_ERROR   1
#define DEBUG_LEVEL_WARN    2
#define DEBUG_LEVEL_INFO    3
#define DEBUG_LEVEL_DEBUG   4
#define DEBUG_LEVEL_TRACE   5

// Debug output flags
#define DEBUG_FLAG_TIMESTAMP    (1 << 0)
#define DEBUG_FLAG_LOCATION     (1 << 1)
#define DEBUG_FLAG_REGISTERS    (1 << 2)
#define DEBUG_FLAG_STACK_TRACE  (1 << 3)
#define DEBUG_FLAG_MEMORY_DUMP  (1 << 4)

// Stack trace limits
#define MAX_STACK_FRAMES        32
#define STACK_TRACE_DEPTH       16

// Memory dump limits
#define MEMORY_DUMP_BYTES       256
#define MEMORY_DUMP_WIDTH       16

// CPU register state structure (extended from IDT)
struct cpu_registers {
    // General purpose registers
    uint64_t rax, rbx, rcx, rdx;
    uint64_t rsi, rdi, rbp, rsp;
    uint64_t r8, r9, r10, r11;
    uint64_t r12, r13, r14, r15;
    uint64_t rip, rflags;

    // Segment registers
    uint16_t cs, ds, es, fs, gs, ss;

    // Control registers
    uint64_t cr0, cr2, cr3, cr4;

    // Debug registers
    uint64_t dr0, dr1, dr2, dr3, dr6, dr7;

    // MSRs
    uint64_t msr_efer;
    uint64_t msr_star;
    uint64_t msr_lstar;
    uint64_t msr_cstar;
    uint64_t msr_sfmask;
    uint64_t msr_gsbase;
    uint64_t msr_kernelgsbase;
};

// Stack frame structure for stack traces
struct stack_frame {
    uint64_t rbp;
    uint64_t rip;
};

// Memory region descriptor
struct memory_region {
    uint64_t start;
    uint64_t end;
    uint32_t flags;
    char name[32];
};

// Debug context
struct debug_context {
    int debug_level;
    uint32_t debug_flags;
    uint64_t boot_timestamp;
    int initialized;
};

// Debug initialization and control
void debug_init(void);
void debug_set_level(int level);
void debug_set_flags(uint32_t flags);
int debug_get_level(void);
uint32_t debug_get_flags(void);

// Enhanced register dumps
void debug_dump_cpu_state(void);
void debug_dump_registers_from_frame(struct interrupt_frame *frame);
void debug_dump_all_registers(struct cpu_registers *regs);
void debug_dump_control_registers(void);
void debug_dump_segment_registers(void);
void debug_dump_debug_registers(void);
void debug_dump_msr_registers(void);

// Stack trace functions
void debug_dump_stack_trace(void);
void debug_dump_stack_trace_from_frame(struct interrupt_frame *frame);
void debug_dump_stack_trace_from_rbp(uint64_t rbp);
int debug_walk_stack(struct stack_frame *frames, int max_frames);
void debug_print_stack_frame(int frame_num, uint64_t rip, uint64_t rbp);

// Memory analysis
void debug_dump_memory(uint64_t address, size_t length);
void debug_dump_memory_around_rip(uint64_t rip);
void debug_dump_stack_memory(uint64_t rsp, size_t length);
int debug_is_valid_memory_address(uint64_t address);
int debug_get_memory_type(uint64_t address);
void debug_flush(void);

// Exception analysis
void debug_analyze_exception(struct interrupt_frame *frame);
void debug_analyze_page_fault(struct interrupt_frame *frame);
void debug_analyze_general_protection(struct interrupt_frame *frame);
void debug_analyze_double_fault(struct interrupt_frame *frame);

// Memory regions for debugging
void debug_register_memory_region(uint64_t start, uint64_t end, uint32_t flags, const char *name);
struct memory_region *debug_find_memory_region(uint64_t address);
void debug_dump_memory_regions(void);

// Utility functions
uint64_t debug_get_timestamp(void);
void debug_print_timestamp(void);
void debug_print_location(const char *file, int line, const char *function);
void debug_hexdump(const void *data, size_t length, uint64_t base_address);

// Debug macros
#define DEBUG_PRINT(level, ...) \
    do { \
        if (debug_get_level() >= (level)) { \
            if (debug_get_flags() & DEBUG_FLAG_TIMESTAMP) debug_print_timestamp(); \
            kprint(__VA_ARGS__); \
        } \
    } while(0)

#define DEBUG_ERROR(...) DEBUG_PRINT(DEBUG_LEVEL_ERROR, __VA_ARGS__)
#define DEBUG_WARN(...) DEBUG_PRINT(DEBUG_LEVEL_WARN, __VA_ARGS__)
#define DEBUG_INFO(...) DEBUG_PRINT(DEBUG_LEVEL_INFO, __VA_ARGS__)
#define DEBUG_DEBUG(...) DEBUG_PRINT(DEBUG_LEVEL_DEBUG, __VA_ARGS__)
#define DEBUG_TRACE(...) DEBUG_PRINT(DEBUG_LEVEL_TRACE, __VA_ARGS__)

// Location-aware debug macros
#define DEBUG_HERE() debug_print_location(__FILE__, __LINE__, __func__)

// Assertion macros
#define DEBUG_ASSERT(condition) \
    do { \
        if (!(condition)) { \
            DEBUG_ERROR("ASSERTION FAILED: %s at %s:%d\n", #condition, __FILE__, __LINE__); \
            debug_dump_cpu_state(); \
            debug_dump_stack_trace(); \
        } \
    } while(0)

// Conditional debugging
#ifdef DEBUG_BUILD
    #define DEBUG_ONLY(code) code
    #define NDEBUG_ONLY(code)
#else
    #define DEBUG_ONLY(code)
    #define NDEBUG_ONLY(code) code
#endif

#endif // DEBUG_H
