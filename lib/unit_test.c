#include "unit_test.h"
#include "../drivers/serial.h"
#include "stacktrace.h"

static const char *fallback_case_name = "<unnamed>";

static void zero_stats(struct unit_test_stats *stats) {
    if (!stats) {
        return;
    }

    stats->total_cases = 0;
    stats->passed_cases = 0;
    stats->failed_cases = 0;
    stats->skipped_cases = 0;
    stats->expected_failures = 0;
    stats->unexpected_failures = 0;
}

void unit_test_runner_init(struct unit_test_runner *runner,
                           const char *name,
                           struct unit_test_stats *stats) {
    if (!runner) {
        return;
    }

    runner->name = name;
    runner->current_case = NULL;
    runner->stats = stats;

    zero_stats(stats);
}

void unit_test_runner_begin_case(struct unit_test_runner *runner,
                                 const char *case_name) {
    if (!runner || !runner->stats) {
        return;
    }

    runner->current_case = case_name ? case_name : fallback_case_name;
    runner->stats->total_cases++;
}

void unit_test_runner_finish_case(struct unit_test_runner *runner,
                                  enum unit_test_status status) {
    if (!runner || !runner->stats) {
        return;
    }

    switch (status) {
        case UNIT_TEST_STATUS_PASS:
            runner->stats->passed_cases++;
            break;
        case UNIT_TEST_STATUS_SKIP:
            runner->stats->skipped_cases++;
            break;
        case UNIT_TEST_STATUS_EXPECTED_FAIL:
            runner->stats->failed_cases++;
            runner->stats->expected_failures++;
            break;
        case UNIT_TEST_STATUS_FAIL:
        default:
            runner->stats->failed_cases++;
            runner->stats->unexpected_failures++;
            if (runner->current_case) {
                kprint("UNIT_TEST: Case '");
                kprint(runner->current_case);
                kprintln("' failed");
            } else {
                kprintln("UNIT_TEST: Case failure detected");
            }
            stacktrace_dump(8);
            break;
    }

    runner->current_case = NULL;
}

const char *unit_test_status_string(enum unit_test_status status) {
    switch (status) {
        case UNIT_TEST_STATUS_PASS:
            return "pass";
        case UNIT_TEST_STATUS_FAIL:
            return "fail";
        case UNIT_TEST_STATUS_SKIP:
            return "skip";
        case UNIT_TEST_STATUS_EXPECTED_FAIL:
            return "xfail";
        default:
            return "unknown";
    }
}

void unit_test_runner_report(const struct unit_test_runner *runner) {
    if (!runner || !runner->stats) {
        return;
    }

    kprintln("=== UNIT TEST SUMMARY ===");

    if (runner->name) {
        kprint("Suite: ");
        kprintln(runner->name);
    }

    kprint("Total cases: ");
    kprint_decimal(runner->stats->total_cases);
    kprintln("");

    kprint("Passed: ");
    kprint_decimal(runner->stats->passed_cases);
    kprintln("");

    kprint("Failed: ");
    kprint_decimal(runner->stats->failed_cases);
    kprintln("");

    if (runner->stats->skipped_cases != 0) {
        kprint("Skipped: ");
        kprint_decimal(runner->stats->skipped_cases);
        kprintln("");
    }

    if (runner->stats->expected_failures != 0) {
        kprint("Expected failures: ");
        kprint_decimal(runner->stats->expected_failures);
        kprintln("");
    }

    if (runner->stats->unexpected_failures != 0) {
        kprint("Unexpected failures: ");
        kprint_decimal(runner->stats->unexpected_failures);
        kprintln("");
    }

    kprintln("=== END UNIT TEST SUMMARY ===");
}

int unit_test_run_suite(struct unit_test_runner *runner,
                        const struct unit_test_suite *suite) {
    if (!runner || !suite || !suite->cases || suite->case_count == 0) {
        return 0;
    }

    int passed = 0;

    for (size_t index = 0; index < suite->case_count; index++) {
        const struct unit_test_case *test_case = &suite->cases[index];
        enum unit_test_status status = UNIT_TEST_STATUS_SKIP;

        unit_test_runner_begin_case(runner, test_case->name);

        if (test_case->execute) {
            status = test_case->execute(test_case->context);
        }

        if (status == UNIT_TEST_STATUS_PASS) {
            passed++;
        }

        unit_test_runner_finish_case(runner, status);
    }

    return passed;
}
