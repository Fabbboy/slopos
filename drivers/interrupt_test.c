/*
 * SlopOS Interrupt Test Framework
 * Controlled exception generation and interrupt testing
 */

#include "interrupt_test.h"
#include "serial.h"
#include "apic.h"
#include "../boot/idt.h"
#include "../boot/constants.h"
#include "../boot/shutdown.h"
#include "../mm/kernel_heap.h"
#include "../mm/paging.h"
#include "../mm/phys_virt.h"
#include "../lib/cpu.h"
#include "../lib/io.h"
#include "../lib/memory.h"
#include <stddef.h>
#include <stdint.h>

// Global test state
static struct test_context test_ctx = {0};
static struct test_stats test_statistics = {0};
static uint32_t test_flags = 0;
static struct interrupt_test_config active_config = {
    .enabled = 0,
    .verbosity = INTERRUPT_TEST_VERBOSITY_SUMMARY,
    .suite_mask = INTERRUPT_TEST_SUITE_ALL,
    .timeout_ms = 0,
    .shutdown_on_complete = 0,
    .stacktrace_demo = 0,
};
static uint64_t estimated_cycles_per_ms = 0;
static uint64_t test_timeout_cycles = 0;

#define TEST_MAX_EXCEPTION_DEPTH 8
#define TEST_STATE_GUARD 0xC0DECAFEu

enum test_recovery_reason {
    TEST_RECOVERY_NONE = 0,
    TEST_RECOVERY_DEPTH_OVERFLOW,
    TEST_RECOVERY_FRAME_CORRUPTION,
    TEST_RECOVERY_INVALID_FRAME,
};

struct saved_exception_state {
    struct interrupt_frame snapshot;
    uint32_t checksum;
    uint32_t guard;
};

static struct saved_exception_state exception_state_stack[TEST_MAX_EXCEPTION_DEPTH];

static void reset_exception_state(void);
static struct saved_exception_state *push_exception_state(struct interrupt_frame *frame);
static void pop_exception_state(void);
static uint32_t compute_frame_checksum(const struct interrupt_frame *frame);
static int validate_exception_state(struct saved_exception_state *slot,
                                    struct interrupt_frame *frame);
static void handle_exception_recovery(enum test_recovery_reason reason,
                                      struct interrupt_frame *frame,
                                      const struct saved_exception_state *slot);
static const char *recovery_reason_string(enum test_recovery_reason reason);

static void reset_test_statistics(void) {
    test_statistics.total_cases = 0;
    test_statistics.passed_cases = 0;
    test_statistics.failed_cases = 0;
    test_statistics.exceptions_caught = 0;
    test_statistics.unexpected_exceptions = 0;
    test_statistics.elapsed_ms = 0;
    test_statistics.timed_out = 0;
}

static uint64_t estimate_cycles_per_ms(void) {
    uint32_t eax = 0;
    uint32_t ebx = 0;
    uint32_t ecx = 0;
    uint32_t edx = 0;

    cpuid(0, &eax, &ebx, &ecx, &edx);
    if (eax >= 0x16) {
        cpuid(0x16, &eax, &ebx, &ecx, &edx);
        if (eax != 0) {
            return (uint64_t)eax * 1000ULL;
        }
    }

    /* Fallback assumption: 3 GHz base frequency */
    return 3000000ULL;
}

static void refresh_timeout_cache(void) {
    if (active_config.timeout_ms == 0) {
        test_timeout_cycles = 0;
        return;
    }

    if (estimated_cycles_per_ms == 0) {
        estimated_cycles_per_ms = estimate_cycles_per_ms();
        if (estimated_cycles_per_ms == 0) {
            estimated_cycles_per_ms = 3000000ULL;
        }
    }

    test_timeout_cycles = estimated_cycles_per_ms * (uint64_t)active_config.timeout_ms;
    if (test_timeout_cycles == 0) {
        /* Prevent zero timeout due to overflow */
        test_timeout_cycles = (uint64_t)active_config.timeout_ms * 3000000ULL;
    }
}

static uint64_t cycles_to_ms(uint64_t cycles) {
    if (estimated_cycles_per_ms == 0) {
        return 0;
    }
    return cycles / estimated_cycles_per_ms;
}

static void zero_interrupt_frame(struct interrupt_frame *frame) {
    if (!frame) {
        return;
    }

    memset(frame, 0, sizeof(struct interrupt_frame));
}

static void copy_interrupt_frame(struct interrupt_frame *dst,
                                 const struct interrupt_frame *src) {
    if (!dst || !src) {
        return;
    }

    memcpy(dst, src, sizeof(struct interrupt_frame));
}

static void copy_saved_exception_state(struct saved_exception_state *dst,
                                       const struct saved_exception_state *src) {
    if (!dst || !src) {
        return;
    }

    copy_interrupt_frame(&dst->snapshot, &src->snapshot);
    dst->checksum = src->checksum;
    dst->guard = src->guard;
}

static void reset_exception_state(void) {
    for (int i = 0; i < TEST_MAX_EXCEPTION_DEPTH; i++) {
        zero_interrupt_frame(&exception_state_stack[i].snapshot);
        exception_state_stack[i].checksum = 0;
        exception_state_stack[i].guard = 0;
    }
    test_ctx.exception_depth = 0;
    test_ctx.context_corrupted = 0;
    test_ctx.abort_requested = 0;
    test_ctx.last_recovery_reason = TEST_RECOVERY_NONE;
    test_ctx.recovery_rip = 0;
}

static uint32_t compute_frame_checksum(const struct interrupt_frame *frame) {
    if (!frame) {
        return 0;
    }

    struct interrupt_frame temp;
    copy_interrupt_frame(&temp, frame);
    temp.rip = 0;

    const uint64_t *words = (const uint64_t *)&temp;
    size_t count = sizeof(temp) / sizeof(uint64_t);
    uint64_t hash = 0xCBF29CE484222325ULL;  // FNV offset basis

    for (size_t i = 0; i < count; i++) {
        hash ^= words[i];
        hash *= 0x100000001B3ULL;  // FNV prime
    }

    return (uint32_t)(hash ^ (hash >> 32));
}

static struct saved_exception_state *push_exception_state(struct interrupt_frame *frame) {
    if (!frame) {
        return NULL;
    }

    if (test_ctx.exception_depth >= TEST_MAX_EXCEPTION_DEPTH) {
        return NULL;
    }

    struct saved_exception_state *slot = &exception_state_stack[test_ctx.exception_depth];
    copy_interrupt_frame(&slot->snapshot, frame);
    slot->checksum = compute_frame_checksum(frame);
    slot->guard = TEST_STATE_GUARD;

    test_ctx.exception_depth++;
    return slot;
}

static void pop_exception_state(void) {
    if (test_ctx.exception_depth <= 0) {
        return;
    }

    test_ctx.exception_depth--;

    struct saved_exception_state *slot = &exception_state_stack[test_ctx.exception_depth];
    zero_interrupt_frame(&slot->snapshot);
    slot->checksum = 0;
    slot->guard = 0;
}

static int validate_exception_state(struct saved_exception_state *slot,
                                    struct interrupt_frame *frame) {
    if (!slot || !frame) {
        return 0;
    }

    if (slot->guard != TEST_STATE_GUARD) {
        return 0;
    }

    uint32_t checksum = compute_frame_checksum(frame);
    return checksum == slot->checksum;
}

static const char *recovery_reason_string(enum test_recovery_reason reason) {
    switch (reason) {
        case TEST_RECOVERY_NONE:
            return "none";
        case TEST_RECOVERY_DEPTH_OVERFLOW:
            return "state-stack overflow";
        case TEST_RECOVERY_FRAME_CORRUPTION:
            return "frame corruption detected";
        case TEST_RECOVERY_INVALID_FRAME:
            return "invalid frame pointer";
        default:
            return "unknown";
    }
}

static void handle_exception_recovery(enum test_recovery_reason reason,
                                      struct interrupt_frame *frame,
                                      const struct saved_exception_state *slot) {
    struct interrupt_frame fallback;
    struct interrupt_frame *active_frame = frame;

    if (!active_frame) {
        zero_interrupt_frame(&fallback);
        if (slot) {
            copy_interrupt_frame(&fallback, &slot->snapshot);
        }
        active_frame = &fallback;
    }

    if (slot) {
        copy_interrupt_frame(active_frame, &slot->snapshot);
        if (frame) {
            copy_interrupt_frame(frame, &slot->snapshot);
        }
    }

    test_ctx.abort_requested = 1;
    test_ctx.context_corrupted = 1;
    test_ctx.last_recovery_reason = (int)reason;

    if (!test_ctx.exception_occurred) {
        test_ctx.exception_occurred = 1;
    }
    test_ctx.exception_vector = (int)(active_frame->vector & 0xFF);

    uint64_t next_rip = test_ctx.recovery_rip;
    if (next_rip == 0 && test_ctx.resume_rip != 0) {
        next_rip = test_ctx.resume_rip;
    }
    if (next_rip == 0) {
        next_rip = active_frame->rip + 1;
    }
    if (frame) {
        frame->rip = next_rip;
    }
    active_frame->rip = next_rip;
    test_ctx.resume_rip = 0;

    if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
        kprint("INTERRUPT_TEST: Recovery triggered (");
        kprint(recovery_reason_string(reason));
        kprint(") for vector ");
        kprint_decimal((int)(active_frame->vector & 0xFF));
        kprintln("");
    }
}
static void interrupt_test_apply_config(const struct interrupt_test_config *config) {
    if (config) {
        active_config = *config;
    } else {
        active_config.enabled = 1;
        active_config.verbosity = INTERRUPT_TEST_VERBOSITY_VERBOSE;
        active_config.suite_mask = INTERRUPT_TEST_SUITE_ALL;
        active_config.timeout_ms = 0;
    }

    test_flags = TEST_FLAG_CONTINUE_ON_FAIL;
    if (active_config.verbosity == INTERRUPT_TEST_VERBOSITY_VERBOSE) {
        test_flags |= TEST_FLAG_VERBOSE;
    } else {
        test_flags &= ~TEST_FLAG_VERBOSE;
    }

    refresh_timeout_cache();
}

/* Test case descriptor used for suite execution */
struct interrupt_test_case {
    test_function_t function;
    const char *name;
    int expected_vector;
};

#define TEST_CASE(fn, vec)      { (fn), #fn, (vec) }
#define TEST_CASE_NOEX(fn)      { (fn), #fn, -1 }

static int execute_test_suite(const char *suite_name,
                              const struct interrupt_test_case *cases,
                              size_t count);

/*
 * Initialize interrupt test framework
 */
void interrupt_test_init(const struct interrupt_test_config *config) {
    interrupt_test_apply_config(config);

    if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
        kprintln("INTERRUPT_TEST: Initializing test framework");
    }

    exception_set_mode(EXCEPTION_MODE_TEST);
    reset_exception_state();

    // Clear test context
    test_ctx.test_active = 0;
    test_ctx.expected_exception = -1;
    test_ctx.exception_occurred = 0;
    test_ctx.exception_vector = -1;
    test_ctx.test_rip = 0;
    test_ctx.resume_rip = 0;
    test_ctx.last_frame = NULL;
    test_ctx.recovery_rip = 0;
    test_ctx.abort_requested = 0;
    test_ctx.context_corrupted = 0;
    test_ctx.last_recovery_reason = TEST_RECOVERY_NONE;

    reset_test_statistics();

    // Install our exception handler for testing
    // Skip critical exceptions that should not be overridden
    for (int i = 0; i < 32; i++) {
        if (exception_is_critical((uint8_t)i)) {
            continue;
        }
        idt_install_exception_handler((uint8_t)i, test_exception_handler);
    }

    if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
        kprintln("INTERRUPT_TEST: Framework initialized");
    }
}

/*
 * Cleanup interrupt test framework
 */
void interrupt_test_cleanup(void) {
    if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
        kprintln("INTERRUPT_TEST: Cleaning up test framework");
    }

    // Remove our exception handlers
    for (int i = 0; i < 32; i++) {
        idt_install_exception_handler((uint8_t)i, NULL);
    }

    test_ctx.test_active = 0;
    test_ctx.expected_exception = -1;
    test_ctx.exception_occurred = 0;
    test_ctx.exception_vector = -1;
    test_ctx.test_rip = 0;
    test_ctx.resume_rip = 0;
    test_ctx.last_frame = NULL;
    reset_exception_state();

    exception_set_mode(EXCEPTION_MODE_NORMAL);

    if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
        kprintln("INTERRUPT_TEST: Framework cleanup complete");
    }
}

/*
 * Start a test
 */
void test_start(const char *name, int expected_exception) {
    reset_exception_state();

    test_ctx.test_active = 1;
    test_ctx.expected_exception = expected_exception;
    test_ctx.exception_occurred = 0;
    test_ctx.exception_vector = -1;
    test_ctx.resume_rip = 0;
    test_ctx.last_frame = NULL;
    test_ctx.test_rip = 0;
    test_ctx.recovery_rip = 0;
    test_ctx.abort_requested = 0;
    test_ctx.context_corrupted = 0;
    test_ctx.last_recovery_reason = TEST_RECOVERY_NONE;

    // Copy test name
    int i = 0;
    while (name && name[i] && i < 63) {
        test_ctx.test_name[i] = name[i];
        i++;
    }
    test_ctx.test_name[i] = '\0';

    test_statistics.total_cases++;

    if (test_flags & TEST_FLAG_VERBOSE) {
        kprint("INTERRUPT_TEST: Starting test '");
        kprint(test_ctx.test_name);
        if (expected_exception >= 0) {
            kprint("' (expecting exception ");
            kprint_decimal(expected_exception);
            kprintln(")");
        } else {
            kprintln("' (no exception expected)");
        }
    }
}

/*
 * End a test and return result
 */
int test_end(void) {
    int result = TEST_SUCCESS;
    int recovery_failure = test_ctx.context_corrupted || test_ctx.abort_requested;
    int expected_vector = test_ctx.expected_exception;
    int vector_matches = (test_ctx.exception_vector == expected_vector);
    int exception_seen = test_ctx.exception_occurred;
    int last_reason = test_ctx.last_recovery_reason;

    if (recovery_failure) {
        result = TEST_FAILED;
        if (!exception_seen || expected_vector < 0 || !vector_matches) {
            test_statistics.unexpected_exceptions++;
        }
    } else if (expected_vector >= 0) {
        if (exception_seen && vector_matches) {
            result = TEST_EXCEPTION_CAUGHT;
        } else if (!exception_seen) {
            result = TEST_NO_EXCEPTION;
        } else {
            result = TEST_WRONG_EXCEPTION;
        }
    } else {
        if (exception_seen) {
            result = TEST_FAILED;
            test_statistics.unexpected_exceptions++;
        } else {
            result = TEST_SUCCESS;
        }
    }

    // Log test results (safe to do here after handler has completed)
    if (test_flags & TEST_FLAG_VERBOSE) {
        kprint("INTERRUPT_TEST: Test '");
        kprint(test_ctx.test_name);
        kprint("' ");
        kprint(get_test_result_string(result));

        if (test_ctx.exception_occurred) {
            kprint(" - exception ");
            kprint_decimal(test_ctx.exception_vector);
            kprint(" at RIP ");
            kprint_hex(test_ctx.test_rip);
        }
        if (recovery_failure) {
            kprint(" (recovery: ");
            kprint(recovery_reason_string((enum test_recovery_reason)last_reason));
            kprint(")");
        }
        kprintln("");
    }

    if (result == TEST_SUCCESS || result == TEST_EXCEPTION_CAUGHT) {
        test_statistics.passed_cases++;
    } else {
        test_statistics.failed_cases++;
    }

    test_ctx.test_active = 0;
    test_ctx.resume_rip = 0;
    test_ctx.recovery_rip = 0;
    test_ctx.last_frame = NULL;
    test_ctx.test_rip = 0;
    test_ctx.expected_exception = -1;
    reset_exception_state();
    return result;
}

/*
 * Update expected exception for current test
 */
void test_expect_exception(int vector) {
    if (!test_ctx.test_active) {
        test_ctx.expected_exception = vector;
        return;
    }

    test_ctx.expected_exception = vector;
    test_ctx.exception_occurred = 0;
    test_ctx.exception_vector = -1;
    test_ctx.resume_rip = 0;
    test_ctx.abort_requested = 0;
    test_ctx.context_corrupted = 0;
    test_ctx.last_recovery_reason = TEST_RECOVERY_NONE;
}

/*
 * Test exception handler
 * This handler is called during test execution and must be very careful
 * to avoid causing secondary faults
 */
void test_exception_handler(struct interrupt_frame *frame) {
    if (!frame) {
        handle_exception_recovery(TEST_RECOVERY_INVALID_FRAME, frame, NULL);
        return;
    }

    struct saved_exception_state *slot = push_exception_state(frame);
    if (!slot) {
        handle_exception_recovery(TEST_RECOVERY_DEPTH_OVERFLOW, frame, NULL);
        return;
    }

    int vector = (int)(frame->vector & 0xFF);

    if (test_ctx.test_active) {
        test_ctx.exception_occurred = 1;
        test_ctx.exception_vector = vector;
        test_ctx.last_frame = frame;
        test_ctx.test_rip = slot->snapshot.rip;
        test_statistics.exceptions_caught++;

        if (test_ctx.resume_rip != 0) {
            frame->rip = test_ctx.resume_rip;
            test_ctx.resume_rip = 0;
        } else {
            switch (vector) {
                case EXCEPTION_INVALID_OPCODE:
                    frame->rip += 2;
                    break;
                case EXCEPTION_BREAKPOINT:
                    frame->rip += 1;
                    break;
                case EXCEPTION_PAGE_FAULT:
                case EXCEPTION_GENERAL_PROTECTION:
                    frame->rip += 1;
                    break;
                default:
                    frame->rip += 1;
                    break;
            }
        }
    }

    if (!validate_exception_state(slot, frame)) {
        struct saved_exception_state saved_copy;
        copy_saved_exception_state(&saved_copy, slot);
        handle_exception_recovery(TEST_RECOVERY_FRAME_CORRUPTION, frame, &saved_copy);
        pop_exception_state();
        return;
    }

    pop_exception_state();
}

/*
 * Safe test execution wrapper
 */
int safe_execute_test(test_function_t test_func, const char *test_name, int expected_exception) {
    test_start(test_name, expected_exception);

#if defined(__clang__)
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wgnu-label-as-value"
#endif
    void *recovery_anchor = &&test_recovery_anchor;
#if defined(__clang__)
#pragma clang diagnostic pop
#endif
    test_ctx.recovery_rip = (uint64_t)(uintptr_t)recovery_anchor;
    test_ctx.abort_requested = 0;

    test_func();

test_recovery_anchor:
    test_ctx.recovery_rip = 0;
    return test_end();
}

/*
 * Execute a suite of tests and report aggregated results
 */
static int execute_test_suite(const char *suite_name,
                              const struct interrupt_test_case *cases,
                              size_t count) {
    if (!cases || count == 0) {
        return 0;
    }

    if (suite_name && (test_flags & TEST_FLAG_VERBOSE)) {
        kprint("INTERRUPT_TEST: Running suite '");
        kprint(suite_name);
        kprintln("'");
    }

    int passed = 0;

    for (size_t i = 0; i < count; i++) {
        int result = safe_execute_test(cases[i].function,
                                       cases[i].name,
                                       cases[i].expected_vector);
        if (result == TEST_SUCCESS || result == TEST_EXCEPTION_CAUGHT) {
            passed++;
        } else if (!(test_flags & TEST_FLAG_CONTINUE_ON_FAIL)) {
            if (suite_name) {
                kprint("INTERRUPT_TEST: Aborting suite '");
                kprint(suite_name);
                kprintln("' due to failure");
            }
            break;
        }
    }

    if (suite_name && (test_flags & TEST_FLAG_VERBOSE)) {
        kprint("INTERRUPT_TEST: Suite '");
        kprint(suite_name);
        kprint("' - ");
        kprint_decimal(passed);
        kprint(" / ");
        kprint_decimal((int)count);
        kprintln(" tests passed");
    }

    return passed;
}

/*
 * Test memory allocation tracking for cleanup
 */
struct test_allocation_header {
    void *raw_ptr;
    size_t size;
};

static struct test_allocation_header *get_allocation_header(void *ptr) {
    if (!ptr) {
        return NULL;
    }

    uintptr_t addr = (uintptr_t)ptr;
    if (addr < sizeof(struct test_allocation_header)) {
        return NULL;
    }

    return (struct test_allocation_header*)(addr - sizeof(struct test_allocation_header));
}

/*
 * Allocate aligned test memory with optional zeroing
 */
void *allocate_test_memory(size_t size, int flags) {
    if (size == 0) {
        size = PAGE_SIZE_4KB;
    }

    size_t aligned_size = (size + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);
    size_t total_size = aligned_size + PAGE_SIZE_4KB + sizeof(struct test_allocation_header);

    uint8_t *raw = kmalloc(total_size);
    if (!raw) {
        kprintln("INTERRUPT_TEST: allocate_test_memory failed");
        return NULL;
    }

    uintptr_t base = (uintptr_t)(raw + sizeof(struct test_allocation_header));
    uintptr_t aligned_addr = (base + PAGE_SIZE_4KB - 1) & ~(PAGE_SIZE_4KB - 1);

    struct test_allocation_header *header =
        (struct test_allocation_header*)(aligned_addr - sizeof(struct test_allocation_header));
    header->raw_ptr = raw;
    header->size = aligned_size;

    if (flags & TEST_MEM_FLAG_ZERO) {
        memset((void *)aligned_addr, 0, aligned_size);
    }

    return (void*)aligned_addr;
}

/*
 * Free test memory allocation
 */
void free_test_memory(void *ptr) {
    if (!ptr) {
        return;
    }

    struct test_allocation_header *header = get_allocation_header(ptr);
    if (!header || !header->raw_ptr) {
        return;
    }

    kfree(header->raw_ptr);
}

/*
 * Map a single 4KB test page with specified flags
 */
int map_test_memory(uint64_t vaddr, uint64_t paddr, int flags) {
    if ((vaddr & (PAGE_SIZE_4KB - 1)) || (paddr & (PAGE_SIZE_4KB - 1))) {
        return -1;
    }
    return map_page_4kb(vaddr, paddr, (uint64_t)flags);
}

/*
 * Unmap a single 4KB test page
 */
int unmap_test_memory(uint64_t vaddr) {
    if (vaddr & (PAGE_SIZE_4KB - 1)) {
        return -1;
    }
    return unmap_page(vaddr);
}

/*
 * Test: Regular kernel memory access (no exception expected)
 */
__attribute__((noinline)) int test_kernel_memory_access(void) {
    uint8_t *buffer = allocate_test_memory(PAGE_SIZE_4KB, TEST_MEM_FLAG_ZERO);
    if (!buffer) {
        return TEST_FAILED;
    }

    for (size_t i = 0; i < 64; i++) {
        buffer[i] = (uint8_t)(i & 0xFF);
    }

    volatile uint32_t accumulator = 0;
    for (size_t i = 0; i < 64; i++) {
        accumulator += buffer[i];
    }
    (void)accumulator;

    free_test_memory(buffer);
    return TEST_SUCCESS;
}

/*
 * Test: Access unmapped memory page (expect page fault)
 */
__attribute__((noinline)) int test_unmapped_memory_access(void) {
    uint8_t *buffer = allocate_test_memory(PAGE_SIZE_4KB, TEST_MEM_FLAG_ZERO);
    if (!buffer) {
        return TEST_FAILED;
    }

    uint64_t phys_addr = mm_virt_to_phys((uint64_t)buffer);
    if (!phys_addr) {
        free_test_memory(buffer);
        return TEST_FAILED;
    }

    volatile uint64_t resume_addr;
    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");

    test_ctx.resume_rip = resume_addr;

    int unmapped = 0;
    int result = TEST_SUCCESS;

    if (unmap_test_memory((uint64_t)buffer) == 0) {
        unmapped = 1;

        __asm__ volatile (
            "mov %0, %%rsi\n\t"
            "movb (%%rsi), %%al\n\t"
            "1:\n\t"
            "nop\n\t"
            :
            : "r"(buffer)
            : "rsi", "rax", "memory");
    } else {
        result = TEST_FAILED;
    }

    test_clear_resume_point();

    if (unmapped) {
        if (map_test_memory((uint64_t)buffer, phys_addr, PAGE_KERNEL_RW) != 0) {
            result = TEST_FAILED;
        }
    }

    free_test_memory(buffer);
    return result;
}

/*
 * Test: Write to read-only mapped page (expect page fault)
 */
__attribute__((noinline)) int test_readonly_memory_write(void) {
    uint8_t *buffer = allocate_test_memory(PAGE_SIZE_4KB, TEST_MEM_FLAG_ZERO);
    if (!buffer) {
        return TEST_FAILED;
    }

    uint64_t phys_addr = mm_virt_to_phys((uint64_t)buffer);
    if (!phys_addr) {
        free_test_memory(buffer);
        return TEST_FAILED;
    }

    int ro_mapped = 0;
    int page_unmapped = 0;
    int result = TEST_SUCCESS;

    if (unmap_test_memory((uint64_t)buffer) == 0) {
        page_unmapped = 1;

        if (map_test_memory((uint64_t)buffer, phys_addr, PAGE_KERNEL_RO) == 0) {
            ro_mapped = 1;

            volatile uint64_t resume_addr;
            __asm__ volatile (
                "leaq 1f(%%rip), %0\n\t"
                : "=r"(resume_addr)
                :
                : "memory");

            test_ctx.resume_rip = resume_addr;

            __asm__ volatile (
                "movb $0xAB, (%0)\n\t"
                "1:\n\t"
                "nop\n\t"
                :
                : "r"(buffer)
                : "memory");

            test_clear_resume_point();
        } else {
            result = TEST_FAILED;
        }
    } else {
        result = TEST_FAILED;
    }

    if (ro_mapped) {
        if (unmap_test_memory((uint64_t)buffer) != 0 ||
            map_test_memory((uint64_t)buffer, phys_addr, PAGE_KERNEL_RW) != 0) {
            result = TEST_FAILED;
        }
    } else if (page_unmapped) {
        if (map_test_memory((uint64_t)buffer, phys_addr, PAGE_KERNEL_RW) != 0) {
            result = TEST_FAILED;
        }
    }

    free_test_memory(buffer);
    return result;
}

/*
 * Test: Execute code from dynamically allocated memory (no exception expected)
 */
__attribute__((noinline)) int test_executable_memory_access(void) {
    typedef int (*exec_test_func_t)(void);

    uint8_t *buffer = allocate_test_memory(PAGE_SIZE_4KB, TEST_MEM_FLAG_ZERO);
    if (!buffer) {
        return TEST_FAILED;
    }

    buffer[0] = 0xB8;  /* mov eax, 0x42 */
    buffer[1] = 0x42;
    buffer[2] = 0x00;
    buffer[3] = 0x00;
    buffer[4] = 0x00;
    buffer[5] = 0xC3;  /* ret */

    exec_test_func_t func = (exec_test_func_t)buffer;
    volatile int value = func();
    (void)value;

    free_test_memory(buffer);
    return TEST_SUCCESS;
}

/*
 * Test: Jump to invalid instruction pointer (expect page fault)
 */
__attribute__((noinline)) int test_invalid_instruction_pointer(void) {
    volatile uint64_t resume_addr;
    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");

    test_ctx.resume_rip = resume_addr;

    __asm__ volatile (
        "movabs $0xDEADBEEF, %%rax\n\t"
        "jmp *%%rax\n\t"
        "1:\n\t"
        "nop\n\t"
        :
        :
        : "rax", "memory");

    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Test: Trigger general protection via explicit software interrupt
 */
__attribute__((noinline)) int test_privilege_violation(void) {
    volatile uint64_t resume_addr;

    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");

    test_ctx.resume_rip = resume_addr;

    __asm__ volatile (
        "int $13\n\t"
        "1:\n\t"
        "nop\n\t"
        :
        :
        : "memory");

    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Test: Load invalid segment selector (expect general protection fault)
 */
__attribute__((noinline)) int test_segment_violation(void) {
    volatile uint64_t resume_addr;
    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");

    test_ctx.resume_rip = resume_addr;

    __asm__ volatile (
        "movw $0x0000, %%ax\n\t"
        "movw %%ax, %%fs\n\t"
        "1:\n\t"
        "nop\n\t"
        :
        :
        : "rax", "memory");

    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Test: Divide by zero
 */
__attribute__((noinline)) int test_divide_by_zero(void) {
#if defined(__clang__)
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wgnu-label-as-value"
#endif
    __label__ resume_point;
    void *resume = &&resume_point;
    if (0) goto *resume;
#if defined(__clang__)
#pragma clang diagnostic pop
#endif

    test_set_resume_point(resume);
    __asm__ volatile (
        "movl $1, %%eax\n\t"
        "xorl %%edx, %%edx\n\t"
        "movl $0, %%ecx\n\t"
        "idivl %%ecx\n\t"
        :
        :
        : "rax", "rdx", "rcx", "memory", "cc");
resume_point:
    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Demo helper: trigger divide-by-zero without announcing expectation
 * Used to exercise stack trace reporting on unexpected failures
 */
__attribute__((noinline)) static int test_stacktrace_demo_fault(void) {
    kprintln("INTERRUPT_TEST: Stacktrace demo triggering divide-by-zero");
    return test_divide_by_zero();
}

/*
 * Test: Invalid opcode  
 */
__attribute__((noinline)) int test_invalid_opcode(void) {
    // Use fallback - handler will skip the UD2 instruction (2 bytes)
    test_ctx.resume_rip = 0;  // Let handler use instruction length
    
    __asm__ volatile (
        "ud2\n\t"
        "nop\n\t"
        :
        :
        : "memory");
    
    return TEST_SUCCESS;
}

/*
 * Test: Page fault (read from unmapped memory)
 */
__attribute__((noinline)) int test_page_fault_read(void) {
    volatile uint64_t resume_addr;
    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");
    
    test_ctx.resume_rip = resume_addr;
    
    __asm__ volatile (
        "movabs $0xDEADBEEF, %%rsi\n\t"
        "movb (%%rsi), %%al\n\t"
        "1:\n\t"
        "nop\n\t"
        :
        :
        : "rax", "rsi", "memory");
    
    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Test: Page fault (write to unmapped memory)
 */
__attribute__((noinline)) int test_page_fault_write(void) {
    volatile uint64_t resume_addr;
    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");
    
    test_ctx.resume_rip = resume_addr;
    
    __asm__ volatile (
        "movabs $0xDEADBEEF, %%rsi\n\t"
        "movb $0x42, (%%rsi)\n\t"
        "1:\n\t"
        "nop\n\t"
        :
        :
        : "rsi", "memory");
    
    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Test: General protection fault
 */
__attribute__((noinline)) int test_general_protection_fault(void) {
    volatile uint64_t resume_addr;
    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");
    
    test_ctx.resume_rip = resume_addr;
    
    __asm__ volatile (
        "movw $0x1234, %%ax\n\t"
        "movw %%ax, %%ds\n\t"
        "1:\n\t"
        "nop\n\t"
        :
        :
        : "rax", "memory");
    
    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Test: Breakpoint
 */
__attribute__((noinline)) int test_breakpoint(void) {
    volatile uint64_t resume_addr;
    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");
    
    test_ctx.resume_rip = resume_addr;
    
    __asm__ volatile (
        "int3\n\t"
        "1:\n\t"
        "nop\n\t"
        :
        :
        : "memory");
    
    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Test: Stack overflow (stack fault)
 */
__attribute__((noinline)) int test_stack_overflow(void) {
    volatile uint64_t resume_addr;
    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");
    
    test_ctx.resume_rip = resume_addr;
    
    __asm__ volatile (
        "movq %%rsp, %%rsi\n\t"
        "subq $0x100000, %%rsi\n\t"
        "movb (%%rsi), %%al\n\t"
        "1:\n\t"
        "nop\n\t"
        :
        :
        : "rax", "rsi", "memory");
    
    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Test: Null pointer dereference
 */
__attribute__((noinline)) int test_null_pointer_dereference(void) {
    volatile uint64_t resume_addr;
    __asm__ volatile (
        "leaq 1f(%%rip), %0\n\t"
        : "=r"(resume_addr)
        :
        : "memory");
    
    test_ctx.resume_rip = resume_addr;
    
    __asm__ volatile (
        "xor %%rdi, %%rdi\n\t"
        "movb (%%rdi), %%al\n\t"
        "1:\n\t"
        "nop\n\t"
        :
        :
        : "rax", "rdi", "memory");
    
    test_clear_resume_point();
    return TEST_SUCCESS;
}

/*
 * Run basic exception tests
 */
int run_basic_exception_tests(void) {
    static const struct interrupt_test_case basic_tests[] = {
        TEST_CASE(test_divide_by_zero, EXCEPTION_DIVIDE_ERROR),
        TEST_CASE(test_invalid_opcode, EXCEPTION_INVALID_OPCODE),
        TEST_CASE(test_breakpoint, EXCEPTION_BREAKPOINT),
    };

    size_t count = sizeof(basic_tests) / sizeof(basic_tests[0]);
    return execute_test_suite("Basic Exceptions", basic_tests, count);
}

/*
 * Run memory access tests
 */
int run_memory_access_tests(void) {
    static const struct interrupt_test_case memory_tests[] = {
        TEST_CASE(test_page_fault_read, EXCEPTION_PAGE_FAULT),
        TEST_CASE(test_page_fault_write, EXCEPTION_PAGE_FAULT),
        TEST_CASE(test_null_pointer_dereference, EXCEPTION_PAGE_FAULT),
        TEST_CASE(test_stack_overflow, EXCEPTION_PAGE_FAULT),
        TEST_CASE_NOEX(test_kernel_memory_access),
        TEST_CASE(test_unmapped_memory_access, EXCEPTION_PAGE_FAULT),
        TEST_CASE(test_readonly_memory_write, EXCEPTION_PAGE_FAULT),
        TEST_CASE_NOEX(test_executable_memory_access),
    };

    size_t count = sizeof(memory_tests) / sizeof(memory_tests[0]);
    return execute_test_suite("Memory Access", memory_tests, count);
}

/*
 * Run control flow tests
 */
int run_control_flow_tests(void) {
    static const struct interrupt_test_case control_tests[] = {
        TEST_CASE(test_general_protection_fault, EXCEPTION_GENERAL_PROTECTION),
        TEST_CASE(test_invalid_instruction_pointer, EXCEPTION_PAGE_FAULT),
    };

    size_t count = sizeof(control_tests) / sizeof(control_tests[0]);
    return execute_test_suite("Control Flow", control_tests, count);
}

/*
 * Test: Context switch balance verification
 * Runs the smoke test and verifies balanced context switch transitions
 */
__attribute__((noinline)) int test_context_switch_balance(void) {
    extern int run_context_switch_smoke_test(void);

    /* Run the smoke test */
    int smoke_result = run_context_switch_smoke_test();
    if (smoke_result != 0) {
        kprint("CONTEXT_SWITCH_TEST: Smoke test failed\n");
        return TEST_FAILED;
    }

    /* For now, just verify the smoke test passed - full scheduler balance test TODO */
    kprint("CONTEXT_SWITCH_TEST: PASSED - Basic context switch test completed\n");
    return TEST_SUCCESS;
}

/*
 * Run scheduler tests (context switch discipline)
 */
int run_scheduler_tests(void) {
    kprint("INTERRUPT_TEST: Running scheduler tests\n");

    int total_passed = 0;

    /* Run the smoke test directly to avoid test framework issues */
    extern int run_context_switch_smoke_test(void);
    int result = run_context_switch_smoke_test();
    if (result == 0) {
        total_passed++;
    } else {
        kprint("INTERRUPT_TEST: Context switch smoke test failed\n");
    }

    /* Run VM manager regression tests */
    extern int run_vm_manager_tests(void);
    int vm_tests_passed = run_vm_manager_tests();
    if (vm_tests_passed > 0) {
        total_passed += vm_tests_passed;
    } else {
        kprint("INTERRUPT_TEST: VM manager tests failed\n");
    }

    /* Run kernel heap regression tests */
    extern int run_kernel_heap_tests(void);
    int heap_tests_passed = run_kernel_heap_tests();
    if (heap_tests_passed > 0) {
        total_passed += heap_tests_passed;
    } else {
        kprint("INTERRUPT_TEST: Kernel heap tests failed\n");
    }

    extern int run_ramfs_tests(void);
    int ramfs_tests_passed = run_ramfs_tests();
    if (ramfs_tests_passed > 0) {
        total_passed += ramfs_tests_passed;
    } else {
        kprint("INTERRUPT_TEST: RamFS tests failed\n");
    }

    if (total_passed > 0) {
        kprint("INTERRUPT_TEST: Scheduler tests completed: ");
        kprint_decimal(total_passed);
        kprint(" tests passed\n");
    } else {
        kprint("INTERRUPT_TEST: Scheduler tests failed\n");
    }

    return total_passed;
}

/*
 * Run all interrupt tests
 */
int run_all_interrupt_tests(const struct interrupt_test_config *config) {
    interrupt_test_apply_config(config);

    if (!active_config.enabled) {
        kprintln("INTERRUPT_TEST: Skipping interrupt tests (disabled)");
        return 0;
    }

    if (estimated_cycles_per_ms == 0) {
        estimated_cycles_per_ms = estimate_cycles_per_ms();
        if (estimated_cycles_per_ms == 0) {
            estimated_cycles_per_ms = 3000000ULL;
        }
    }

    if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
        kprintln("INTERRUPT_TEST: Starting interrupt test suites");
    }

    int total_passed = 0;
    int timed_out = 0;
    uint64_t start_cycles = cpu_read_tsc();
    uint64_t end_cycles = start_cycles;

    if (active_config.suite_mask & INTERRUPT_TEST_SUITE_BASIC) {
        total_passed += run_basic_exception_tests();
        end_cycles = cpu_read_tsc();
        if (test_timeout_cycles != 0 &&
            (end_cycles - start_cycles) > test_timeout_cycles) {
            timed_out = 1;
            if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
                kprintln("INTERRUPT_TEST: Timeout reached during basic exception tests");
            }
            goto finish_execution;
        }
    }
    if (active_config.suite_mask & INTERRUPT_TEST_SUITE_MEMORY) {
        total_passed += run_memory_access_tests();
        end_cycles = cpu_read_tsc();
        if (test_timeout_cycles != 0 &&
            (end_cycles - start_cycles) > test_timeout_cycles) {
            timed_out = 1;
            if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
                kprintln("INTERRUPT_TEST: Timeout reached during memory access tests");
            }
            goto finish_execution;
        }
    }
    if (active_config.suite_mask & INTERRUPT_TEST_SUITE_CONTROL) {
        total_passed += run_control_flow_tests();
        end_cycles = cpu_read_tsc();
        if (test_timeout_cycles != 0 &&
            (end_cycles - start_cycles) > test_timeout_cycles) {
            timed_out = 1;
            if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
                kprintln("INTERRUPT_TEST: Timeout reached during control flow tests");
            }
            goto finish_execution;
        }
    }
    if (active_config.suite_mask & INTERRUPT_TEST_SUITE_SCHEDULER) {
        total_passed += run_scheduler_tests();
        end_cycles = cpu_read_tsc();
        if (test_timeout_cycles != 0 &&
            (end_cycles - start_cycles) > test_timeout_cycles) {
            timed_out = 1;
            if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
                kprintln("INTERRUPT_TEST: Timeout reached during scheduler tests");
            }
            goto finish_execution;
        }
    }

    if (active_config.stacktrace_demo) {
        if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
            kprintln("INTERRUPT_TEST: Running stacktrace demonstration");
        }
        safe_execute_test(test_stacktrace_demo_fault,
                          "stacktrace_demo_unexpected_divide",
                          -1);
        end_cycles = cpu_read_tsc();
        if (test_timeout_cycles != 0 &&
            (end_cycles - start_cycles) > test_timeout_cycles) {
            timed_out = 1;
            if (active_config.verbosity != INTERRUPT_TEST_VERBOSITY_QUIET) {
                kprintln("INTERRUPT_TEST: Timeout reached during stacktrace demo");
            }
            goto finish_execution;
        }
    }

finish_execution:
    if (test_flags & TEST_FLAG_VERBOSE) {
        kprint("INTERRUPT_TEST: Aggregate passed tests: ");
        kprint_decimal(total_passed);
        kprintln("");
    }

    uint64_t elapsed_cycles = end_cycles - start_cycles;
    uint64_t elapsed_ms = cycles_to_ms(elapsed_cycles);
    if (elapsed_ms > 0xFFFFFFFFULL) {
        elapsed_ms = 0xFFFFFFFFULL;
    }
    test_statistics.elapsed_ms = (uint32_t)elapsed_ms;
    test_statistics.timed_out = timed_out;

    if (timed_out) {
        kprintln("INTERRUPT_TEST: Execution aborted due to timeout");
    }

    test_report_results();

    return total_passed;
}

/*
 * Set test flags
 */
void test_set_flags(uint32_t flags) {
    test_flags = flags;
}

/*
 * Check if exception is expected
 */
int test_is_exception_expected(void) {
    return test_ctx.test_active && test_ctx.expected_exception >= 0;
}

void test_set_resume_point(void *rip) {
    test_ctx.resume_rip = (uint64_t)rip;
}

void test_clear_resume_point(void) {
    test_ctx.resume_rip = 0;
}

/*
 * Report test results
 */
void test_report_results(void) {
    kprintln("=== INTERRUPT TEST RESULTS ===");
    kprint("Total tests: ");
    kprint_decimal(test_statistics.total_cases);
    kprintln("");

    kprint("Passed: ");
    kprint_decimal(test_statistics.passed_cases);
    kprintln("");

    kprint("Failed: ");
    kprint_decimal(test_statistics.failed_cases);
    kprintln("");

    kprint("Exceptions caught: ");
    kprint_decimal(test_statistics.exceptions_caught);
    kprintln("");

    kprint("Unexpected exceptions: ");
    kprint_decimal(test_statistics.unexpected_exceptions);
    kprintln("");

    if (test_statistics.total_cases > 0) {
        int success_rate = (int)((test_statistics.passed_cases * 100) /
                                 test_statistics.total_cases);
        kprint("Success rate: ");
        kprint_decimal(success_rate);
        kprintln("%");
    }

    kprint("Elapsed (ms): ");
    kprint_decimal(test_statistics.elapsed_ms);
    kprintln("");

    kprint("Timeout triggered: ");
    kprintln(test_statistics.timed_out ? "Yes" : "No");

    kprintln("=== END TEST RESULTS ===");
}


void interrupt_test_request_shutdown(int failed_tests) {
    kprintln("INTERRUPT_TEST: Auto shutdown requested");

    uint8_t exit_value = failed_tests == 0 ? 0 : 1;
    io_outb(0xF4, exit_value);  /* QEMU debug-exit port */

    if (failed_tests == 0) {
        kernel_shutdown("Interrupt tests completed successfully");
    } else {
        kernel_shutdown("Interrupt tests failed");
    }
}

/*
 * Get test statistics
 */
const struct test_stats *test_get_stats(void) {
    return &test_statistics;
}

/*
 * Get test result string
 */
const char *get_test_result_string(int result) {
    switch (result) {
        case TEST_SUCCESS:
            return "PASSED";
        case TEST_EXCEPTION_CAUGHT:
            return "PASSED (exception caught as expected)";
        case TEST_FAILED:
            return "FAILED";
        case TEST_NO_EXCEPTION:
            return "FAILED (expected exception not triggered)";
        case TEST_WRONG_EXCEPTION:
            return "FAILED (wrong exception triggered)";
        default:
            return "UNKNOWN";
    }
}

/*
 * Dump test context for debugging
 */
void dump_test_context(void) {
    kprintln("=== TEST CONTEXT DUMP ===");
    kprint("Test active: ");
    kprintln(test_ctx.test_active ? "Yes" : "No");

    if (test_ctx.test_active) {
        kprint("Test name: ");
        kprint(test_ctx.test_name);
        kprintln("");

        kprint("Expected exception: ");
        if (test_ctx.expected_exception >= 0) {
            kprint_decimal(test_ctx.expected_exception);
        } else {
            kprint("None");
        }
        kprintln("");

        kprint("Exception occurred: ");
        kprintln(test_ctx.exception_occurred ? "Yes" : "No");

        if (test_ctx.exception_occurred) {
            kprint("Exception vector: ");
            kprint_decimal(test_ctx.exception_vector);
            kprintln("");
        }
    }

    kprint("Abort requested: ");
    kprintln(test_ctx.abort_requested ? "Yes" : "No");

    kprint("Context corrupted: ");
    kprintln(test_ctx.context_corrupted ? "Yes" : "No");

    kprint("Exception depth: ");
    kprint_decimal(test_ctx.exception_depth);
    kprintln("");

    kprint("Recovery anchor: ");
    kprint_hex(test_ctx.recovery_rip);
    kprintln("");

    kprint("Last recovery reason: ");
    kprint(recovery_reason_string((enum test_recovery_reason)test_ctx.last_recovery_reason));
    kprintln("");

    kprintln("=== END TEST CONTEXT DUMP ===");
}

/*
 * Log test exception
 */
void log_test_exception(struct interrupt_frame *frame) {
    kprint("TEST_EXCEPTION: Vector ");
    kprint_decimal(frame->vector);
    kprint(" at RIP ");
    kprint_hex(frame->rip);
    kprintln("");
}
