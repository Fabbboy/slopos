/*
 * SlopOS Test Orchestrator Core
 * Central registry and runner for in-kernel test suites.
 */

#include "core.h"

#include "../drivers/wl_currency.h"
#include "../lib/cpu.h"
#include "../lib/klog.h"

#define TESTS_MAX_CYCLES_PER_MS 3000000ULL

struct registered_suite {
    const struct test_suite_desc *desc;
};

static struct registered_suite registry[TESTS_MAX_SUITES];
static size_t registry_count = 0;
static uint64_t cached_cycles_per_ms = 0;

static uint64_t estimate_cycles_per_ms(void) {
    if (cached_cycles_per_ms != 0) {
        return cached_cycles_per_ms;
    }

    uint32_t eax = 0, ebx = 0, ecx = 0, edx = 0;
    cpuid(0, &eax, &ebx, &ecx, &edx);
    if (eax >= 0x16) {
        cpuid(0x16, &eax, &ebx, &ecx, &edx);
        if (eax != 0) {
            cached_cycles_per_ms = (uint64_t)eax * 1000ULL;
            return cached_cycles_per_ms;
        }
    }

    cached_cycles_per_ms = TESTS_MAX_CYCLES_PER_MS;
    return cached_cycles_per_ms;
}

static uint32_t cycles_to_ms(uint64_t cycles) {
    uint64_t cycles_per_ms = estimate_cycles_per_ms();
    if (cycles_per_ms == 0) {
        return 0;
    }
    uint64_t ms = cycles / cycles_per_ms;
    if (ms > 0xFFFFFFFFULL) {
        return 0xFFFFFFFFU;
    }
    return (uint32_t)ms;
}

void tests_reset_registry(void) {
    registry_count = 0;
}

int tests_register_suite(const struct test_suite_desc *desc) {
    if (!desc || !desc->run || registry_count >= TESTS_MAX_SUITES) {
        return -1;
    }
    registry[registry_count++].desc = desc;
    return 0;
}

static void fill_summary_from_result(struct test_run_summary *summary,
                                     const struct test_suite_result *res) {
    if (!summary || !res) {
        return;
    }
    summary->total_tests += res->total;
    summary->passed += res->passed;
    summary->failed += res->failed;
    summary->exceptions_caught += res->exceptions_caught;
    summary->unexpected_exceptions += res->unexpected_exceptions;
    summary->elapsed_ms += res->elapsed_ms;
    if (res->timed_out) {
        summary->timed_out = 1;
    }
}

static void award_wl_for_result(const struct test_suite_result *res) {
    if (!res) {
        return;
    }
    if (res->total == 0) {
        return;
    }
    if (res->failed == 0 && !res->timed_out) {
        wl_award_win();
    } else {
        wl_award_loss();
    }
}

int tests_run_all(const struct interrupt_test_config *config,
                  struct test_run_summary *summary) {
    if (!config) {
        return -1;
    }

    struct test_run_summary local_summary = {0};
    if (!summary) {
        summary = &local_summary;
    } else {
        *summary = (struct test_run_summary){0};
    }

    if (!config->enabled) {
        klog_printf(KLOG_INFO, "TESTS: Harness disabled\n");
        return 0;
    }

    klog_printf(KLOG_INFO, "TESTS: Starting orchestrated suites\n");

    const struct test_suite_desc *desc_list[TESTS_MAX_SUITES] = {0};
    size_t desc_count = registry_count;
    if (desc_count > TESTS_MAX_SUITES) {
        desc_count = TESTS_MAX_SUITES;
    }
    for (size_t i = 0; i < desc_count; i++) {
        desc_list[i] = registry[i].desc;
    }

    uint64_t start_cycles = cpu_read_tsc();
    for (size_t i = 0; i < desc_count; i++) {
        const struct test_suite_desc *desc = desc_list[i];
        if (!desc || !desc->run) {
            continue;
        }
        if ((config->suite_mask & desc->mask_bit) == 0) {
            continue;
        }

        struct test_suite_result res = {0};
        res.name = desc->name;

        desc->run(config, &res);
        award_wl_for_result(&res);

        if (summary->suite_count < TESTS_MAX_SUITES) {
            summary->suites[summary->suite_count++] = res;
        }

        klog_printf(KLOG_INFO,
                    "SUITE%u total=%u pass=%u fail=%u exc=%u unexp=%u elapsed=%u timeout=%u\n",
                    (unsigned)i,
                    res.total,
                    res.passed,
                    res.failed,
                    res.exceptions_caught,
                    res.unexpected_exceptions,
                    res.elapsed_ms,
                    res.timed_out ? 1u : 0u);
        fill_summary_from_result(summary, &res);
    }
    uint64_t end_cycles = cpu_read_tsc();
    uint32_t overall_ms = cycles_to_ms(end_cycles - start_cycles);
    if (overall_ms > summary->elapsed_ms) {
        summary->elapsed_ms = overall_ms;
    }

    klog_printf(KLOG_INFO, "+----------------------+-------+-------+-------+-------+-------+---------+-----+\n");
    klog_printf(KLOG_INFO, "TESTS SUMMARY: total=%u passed=%u failed=%u exceptions=%u unexpected=%u elapsed_ms=%u timed_out=%s\n",
                summary->total_tests,
                summary->passed,
                summary->failed,
                summary->exceptions_caught,
                summary->unexpected_exceptions,
                summary->elapsed_ms,
                summary->timed_out ? "yes" : "no");

    return summary->failed == 0 ? 0 : -1;
}

