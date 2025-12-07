/*
 * SlopOS Test Orchestrator Core
 * Provides suite registration, execution ordering, and unified reporting.
 */

#ifndef TESTS_CORE_H
#define TESTS_CORE_H

#include <stddef.h>
#include <stdint.h>

#include "../drivers/interrupt_test_config.h"

/* Maximum suites we allow to be registered. */
#define TESTS_MAX_SUITES 8

struct test_suite_result {
    const char *name;
    uint32_t total;
    uint32_t passed;
    uint32_t failed;
    uint32_t exceptions_caught;
    uint32_t unexpected_exceptions;
    uint32_t elapsed_ms;
    int timed_out;
};

typedef int (*test_suite_runner_t)(const struct interrupt_test_config *config,
                                   struct test_suite_result *out);

struct test_suite_desc {
    const char *name;
    uint32_t mask_bit;
    test_suite_runner_t run;
};

struct test_run_summary {
    struct test_suite_result suites[TESTS_MAX_SUITES];
    size_t suite_count;
    uint32_t total_tests;
    uint32_t passed;
    uint32_t failed;
    uint32_t exceptions_caught;
    uint32_t unexpected_exceptions;
    uint32_t elapsed_ms;
    int timed_out;
};

/* Registry management */
void tests_reset_registry(void);
int tests_register_suite(const struct test_suite_desc *desc);

/* Execute all registered suites that match the config mask. */
int tests_run_all(const struct interrupt_test_config *config,
                  struct test_run_summary *summary);

#endif /* TESTS_CORE_H */

