/*
 * Interrupt test configuration helpers
 * Parses compile-time defaults and runtime kernel command line options
 */

#include "interrupt_test_config.h"
#include "../lib/string.h"
#include "../lib/numfmt.h"

#define TOKEN_BUFFER_SIZE 128

static enum interrupt_test_verbosity verbosity_from_string(const char *value) {
    if (!value) {
        return INTERRUPT_TEST_VERBOSITY_SUMMARY;
    }
    if (strcasecmp(value, "quiet") == 0) {
        return INTERRUPT_TEST_VERBOSITY_QUIET;
    }
    if (strcasecmp(value, "verbose") == 0) {
        return INTERRUPT_TEST_VERBOSITY_VERBOSE;
    }
    return INTERRUPT_TEST_VERBOSITY_SUMMARY;
}

static uint32_t suite_from_string(const char *value) {
    if (!value) {
        return INTERRUPT_TEST_SUITE_ALL;
    }
    if (strcasecmp(value, "none") == 0 || strcasecmp(value, "off") == 0) {
        return 0;
    }
    if (strcasecmp(value, "all") == 0) {
        return INTERRUPT_TEST_SUITE_ALL;
    }
    if (strcasecmp(value, "basic") == 0) {
        return INTERRUPT_TEST_SUITE_BASIC;
    }
    if (strcasecmp(value, "memory") == 0) {
        return INTERRUPT_TEST_SUITE_MEMORY;
    }
    if (strcasecmp(value, "control") == 0) {
        return INTERRUPT_TEST_SUITE_CONTROL;
    }
    if (strcasecmp(value, "basic+memory") == 0 ||
        strcasecmp(value, "memory+basic") == 0) {
        return INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_MEMORY;
    }
    if (strcasecmp(value, "basic+control") == 0 ||
        strcasecmp(value, "control+basic") == 0) {
        return INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_CONTROL;
    }
    if (strcasecmp(value, "memory+control") == 0 ||
        strcasecmp(value, "control+memory") == 0) {
        return INTERRUPT_TEST_SUITE_MEMORY | INTERRUPT_TEST_SUITE_CONTROL;
    }
    return INTERRUPT_TEST_SUITE_ALL;
}

static int parse_on_off_flag(const char *value, int current) {
    if (!value) {
        return current;
    }

    if (strcasecmp(value, "on") == 0 ||
        strcasecmp(value, "true") == 0 ||
        strcasecmp(value, "yes") == 0 ||
        strcasecmp(value, "enabled") == 0 ||
        strcasecmp(value, "1") == 0) {
        return 1;
    }

    if (strcasecmp(value, "off") == 0 ||
        strcasecmp(value, "false") == 0 ||
        strcasecmp(value, "no") == 0 ||
        strcasecmp(value, "disabled") == 0 ||
        strcasecmp(value, "0") == 0) {
        return 0;
    }

    return current;
}

static void apply_enable_token(struct interrupt_test_config *config,
                               const char *value) {
    if (!config || !value) {
        return;
    }

    if (strcasecmp(value, "on") == 0 || strcasecmp(value, "true") == 0 ||
        strcasecmp(value, "enabled") == 0) {
        config->enabled = 1;
        return;
    }

    if (strcasecmp(value, "off") == 0 || strcasecmp(value, "false") == 0 ||
        strcasecmp(value, "disabled") == 0) {
        config->enabled = 0;
        config->shutdown_on_complete = 0;
        return;
    }

    /* Interpret suite names as implicit enable */
    uint32_t suite = suite_from_string(value);
    if (suite != 0) {
        config->enabled = 1;
        config->suite_mask = suite;
    } else {
        config->enabled = 0;
        config->suite_mask = 0;
        config->shutdown_on_complete = 0;
    }
}

static void process_token(struct interrupt_test_config *config,
                          const char *token) {
    if (!config || !token) {
        return;
    }

    /* Accept both itests.* and interrupt_tests.* prefixes */
    const char *value = NULL;

    if (strncasecmp(token, "itests=", 7) == 0) {
        value = token + 7;
        apply_enable_token(config, value);
        return;
    }

    if (strncasecmp(token, "interrupt_tests=", 16) == 0) {
        value = token + 16;
        apply_enable_token(config, value);
        return;
    }

    if (strncasecmp(token, "itests.suite=", 13) == 0) {
        value = token + 13;
        uint32_t suite = suite_from_string(value);
        config->suite_mask = suite;
        if (suite != 0) {
            config->enabled = 1;
        }
        return;
    }

    if (strncasecmp(token, "interrupt_tests.suite=", 22) == 0) {
        value = token + 22;
        uint32_t suite = suite_from_string(value);
        config->suite_mask = suite;
        if (suite != 0) {
            config->enabled = 1;
        }
        return;
    }

    if (strncasecmp(token, "itests.verbosity=", 17) == 0) {
        value = token + 17;
        config->verbosity = verbosity_from_string(value);
        return;
    }

    if (strncasecmp(token, "interrupt_tests.verbosity=", 26) == 0) {
        value = token + 26;
        config->verbosity = verbosity_from_string(value);
        return;
    }

    if (strncasecmp(token, "itests.timeout=", 15) == 0) {
        value = token + 15;
        uint32_t parsed = config->timeout_ms;
        numfmt_parse_u32(value, &parsed, config->timeout_ms);
        config->timeout_ms = parsed;
        return;
    }

    if (strncasecmp(token, "interrupt_tests.timeout=", 24) == 0) {
        value = token + 24;
        uint32_t parsed = config->timeout_ms;
        numfmt_parse_u32(value, &parsed, config->timeout_ms);
        config->timeout_ms = parsed;
        return;
    }

    if (strncasecmp(token, "itests.shutdown=", 16) == 0) {
        value = token + 16;
        config->shutdown_on_complete = parse_on_off_flag(value, config->shutdown_on_complete);
        return;
    }

    if (strncasecmp(token, "interrupt_tests.shutdown=", 25) == 0) {
        value = token + 25;
        config->shutdown_on_complete = parse_on_off_flag(value, config->shutdown_on_complete);
        return;
    }

    if (strncasecmp(token, "itests.stacktrace_demo=", 23) == 0) {
        value = token + 23;
        config->stacktrace_demo = parse_on_off_flag(value, config->stacktrace_demo);
        return;
    }

    if (strncasecmp(token, "interrupt_tests.stacktrace_demo=", 32) == 0) {
        value = token + 32;
        config->stacktrace_demo = parse_on_off_flag(value, config->stacktrace_demo);
        return;
    }
}

void interrupt_test_config_init_defaults(struct interrupt_test_config *config) {
    if (!config) {
        return;
    }

    config->enabled = INTERRUPT_TESTS_DEFAULT_ENABLED ? 1 : 0;
    config->timeout_ms = (uint32_t)INTERRUPT_TESTS_DEFAULT_TIMEOUT_MS;
    config->verbosity = verbosity_from_string(INTERRUPT_TESTS_DEFAULT_VERBOSITY);
    config->suite_mask = suite_from_string(INTERRUPT_TESTS_DEFAULT_SUITE);
    config->shutdown_on_complete = INTERRUPT_TESTS_DEFAULT_SHUTDOWN ? 1 : 0;
    config->stacktrace_demo = 0;
}

void interrupt_test_config_parse_cmdline(struct interrupt_test_config *config,
                                         const char *cmdline) {
    if (!config || !cmdline) {
        return;
    }

    size_t len = strlen(cmdline);
    if (len == 0) {
        return;
    }

    const char *cursor = cmdline;
    while (*cursor != '\0') {
        while (*cursor != '\0' && isspace_k((int)*cursor)) {
            cursor++;
        }
        if (*cursor == '\0') {
            break;
        }

        char token[TOKEN_BUFFER_SIZE];
        size_t index = 0;

        while (*cursor != '\0' && !isspace_k((int)*cursor)) {
            if (index < (TOKEN_BUFFER_SIZE - 1)) {
                token[index++] = *cursor;
            }
            cursor++;
        }
        token[index] = '\0';

        if (index > 0) {
            process_token(config, token);
        }
    }
}

const char *interrupt_test_verbosity_string(enum interrupt_test_verbosity verbosity) {
    switch (verbosity) {
        case INTERRUPT_TEST_VERBOSITY_QUIET:
            return "quiet";
        case INTERRUPT_TEST_VERBOSITY_VERBOSE:
            return "verbose";
        case INTERRUPT_TEST_VERBOSITY_SUMMARY:
        default:
            return "summary";
    }
}

const char *interrupt_test_suite_string(uint32_t suite_mask) {
    if (suite_mask == 0) {
        return "none";
    }
    if (suite_mask == INTERRUPT_TEST_SUITE_ALL) {
        return "all";
    }
    if (suite_mask == INTERRUPT_TEST_SUITE_BASIC) {
        return "basic";
    }
    if (suite_mask == INTERRUPT_TEST_SUITE_MEMORY) {
        return "memory";
    }
    if (suite_mask == INTERRUPT_TEST_SUITE_CONTROL) {
        return "control";
    }
    if (suite_mask == (INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_MEMORY)) {
        return "basic+memory";
    }
    if (suite_mask == (INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_CONTROL)) {
        return "basic+control";
    }
    if (suite_mask == (INTERRUPT_TEST_SUITE_MEMORY | INTERRUPT_TEST_SUITE_CONTROL)) {
        return "memory+control";
    }
    return "custom";
}
