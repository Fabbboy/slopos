/*
 * Interrupt-focused test suite wrapper for the orchestrator.
 */

#include "interrupt_suite.h"

#include "../drivers/interrupt_test.h"

static int run_interrupt_suite(const struct interrupt_test_config *config,
                               struct test_suite_result *out) {
    if (!config) {
        return -1;
    }

    struct interrupt_test_config scoped = *config;
    scoped.suite_mask &= (INTERRUPT_TEST_SUITE_BASIC |
                          INTERRUPT_TEST_SUITE_MEMORY |
                          INTERRUPT_TEST_SUITE_CONTROL);

    if (scoped.suite_mask == 0) {
        if (out) {
            out->name = "interrupt";
        }
        return 0;
    }

    interrupt_test_init(&scoped);
    run_all_interrupt_tests(&scoped);
    const struct test_stats *stats = test_get_stats();
    interrupt_test_cleanup();

    if (out) {
        out->name = "interrupt";
        if (stats) {
            out->total = stats->total_cases;
            out->passed = stats->passed_cases;
            out->failed = stats->failed_cases;
            out->exceptions_caught = stats->exceptions_caught;
            out->unexpected_exceptions = stats->unexpected_exceptions;
            out->elapsed_ms = stats->elapsed_ms;
            out->timed_out = stats->timed_out;
        }
    }

    if (!stats) {
        return -1;
    }
    return (stats->failed_cases == 0 && !stats->timed_out) ? 0 : -1;
}

const struct test_suite_desc interrupt_suite_desc = {
    .name = "interrupt",
    .mask_bit = (INTERRUPT_TEST_SUITE_BASIC |
                 INTERRUPT_TEST_SUITE_MEMORY |
                 INTERRUPT_TEST_SUITE_CONTROL),
    .run = run_interrupt_suite,
};

