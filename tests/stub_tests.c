/*
 * Stub implementations for test orchestration when built-in tests are disabled.
 */

#include "core.h"
#include "system_suites.h"

void tests_reset_registry(void) {
}

int tests_register_suite(const struct test_suite_desc *desc) {
    (void)desc;
    return 0;
}

void tests_register_system_suites(void) {
}

int tests_run_all(const struct interrupt_test_config *config,
                  struct test_run_summary *summary) {
    (void)config;
    if (summary) {
        summary->suite_count = 0;
        summary->total_tests = 0;
        summary->passed = 0;
        summary->failed = 0;
        summary->exceptions_caught = 0;
        summary->unexpected_exceptions = 0;
        summary->elapsed_ms = 0;
        summary->timed_out = 0;
    }
    return 0;
}

