/*
 * SlopOS Interrupt Test Framework
 * Controlled exception generation and interrupt testing
 */

#ifndef INTERRUPT_TEST_H
#define INTERRUPT_TEST_H

#include <stdint.h>
#include <stddef.h>
#include "../boot/idt.h"
#include "interrupt_test_config.h"

// Test result codes
#define TEST_SUCCESS            0
#define TEST_FAILED            -1
#define TEST_EXCEPTION_CAUGHT  1
#define TEST_NO_EXCEPTION     -2
#define TEST_WRONG_EXCEPTION  -3

// Test flags
#define TEST_FLAG_EXPECT_EXCEPTION  (1 << 0)
#define TEST_FLAG_CONTINUE_ON_FAIL  (1 << 1)
#define TEST_FLAG_VERBOSE           (1 << 2)

// Test memory allocation flags
#define TEST_MEM_FLAG_ZERO          (1 << 0)

// Test statistics
struct test_stats {
    uint32_t total_cases;
    uint32_t passed_cases;
    uint32_t failed_cases;
    uint32_t exceptions_caught;
    uint32_t unexpected_exceptions;
    uint32_t elapsed_ms;
    int timed_out;
};

// Test context for exception handling
struct test_context {
    int test_active;
    int expected_exception;
    int exception_occurred;
    int exception_vector;
    uint64_t test_rip;
    volatile uint64_t resume_rip;
    struct interrupt_frame *last_frame;
    char test_name[64];
    uint64_t recovery_rip;
    int abort_requested;
    int context_corrupted;
    int exception_depth;
    int last_recovery_reason;
};

// Exception test functions
int test_divide_by_zero(void);
int test_invalid_opcode(void);
int test_page_fault_read(void);
int test_page_fault_write(void);
int test_general_protection_fault(void);
int test_breakpoint(void);
int test_stack_overflow(void);
int test_null_pointer_dereference(void);

// Memory access test functions
int test_kernel_memory_access(void);
int test_unmapped_memory_access(void);
int test_readonly_memory_write(void);
int test_executable_memory_access(void);

// Control flow test functions
int test_invalid_instruction_pointer(void);
int test_privilege_violation(void);
int test_segment_violation(void);

// Test framework functions
void interrupt_test_init(const struct interrupt_test_config *config);
void interrupt_test_cleanup(void);
int run_all_interrupt_tests(const struct interrupt_test_config *config);
int run_basic_exception_tests(void);
int run_memory_access_tests(void);
int run_control_flow_tests(void);
int run_scheduler_tests(void);

// Test utilities
void test_start(const char *name, int expected_exception);
int test_end(void);
void test_expect_exception(int vector);
void test_set_flags(uint32_t flags);
int test_is_exception_expected(void);
void test_set_resume_point(void *rip);
void test_clear_resume_point(void);
void test_report_results(void);
const struct test_stats *test_get_stats(void);
void interrupt_test_request_shutdown(int failed_tests);

// Exception handler for tests
void test_exception_handler(struct interrupt_frame *frame);

// Safe execution wrapper
typedef int (*test_function_t)(void);
int safe_execute_test(test_function_t test_func, const char *test_name, int expected_exception);

// Test assertion macros
#define ASSERT_EXCEPTION(vector) test_expect_exception(vector)
#define ASSERT_NO_EXCEPTION() test_expect_exception(-1)

// Test execution macros
#define RUN_TEST(func, expected_exc) safe_execute_test(func, #func, expected_exc)
#define RUN_TEST_NO_EXCEPTION(func) safe_execute_test(func, #func, -1)

// Memory manipulation utilities for testing
void *allocate_test_memory(size_t size, int flags);
void free_test_memory(void *ptr);
int map_test_memory(uint64_t vaddr, uint64_t paddr, int flags);
int unmap_test_memory(uint64_t vaddr);

// Debug utilities
void dump_test_context(void);
void log_test_exception(struct interrupt_frame *frame);
const char *get_test_result_string(int result);

#endif // INTERRUPT_TEST_H
