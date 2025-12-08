/*
 * System-level test suites (VM manager, kernel heap, RAMFS, privilege separation).
 */

#include "system_suites.h"

#include "core.h"
#include "../lib/klog.h"
#include "../lib/cpu.h"
#include "../mm/process_vm.h"
#include "../mm/paging.h"
#include "../mm/memory_layout.h"
#include "../sched/task.h"
#include "../sched/scheduler.h"
#include "../video/roulette.h"

#ifndef ENABLE_BUILTIN_TESTS
#define ENABLE_BUILTIN_TESTS 0
#endif

#if ENABLE_BUILTIN_TESTS
extern int run_vm_manager_tests(void);
extern int run_kernel_heap_tests(void);
extern int run_ramfs_tests(void);
extern int run_privilege_separation_invariant_test(void);
extern void roulette_user_main(void *arg);
#endif
static uint32_t measure_elapsed_ms(uint64_t start_cycles, uint64_t end_cycles) {
    uint64_t cycles = end_cycles - start_cycles;
    uint64_t freq = 3000000ULL;

    uint32_t eax = 0, ebx = 0, ecx = 0, edx = 0;
    cpuid(0, &eax, &ebx, &ecx, &edx);
    if (eax >= 0x16) {
        cpuid(0x16, &eax, &ebx, &ecx, &edx);
        if (eax != 0) {
            freq = (uint64_t)eax * 1000ULL;
        }
    }

    uint64_t ms = cycles / freq;
    if (ms > 0xFFFFFFFFULL) {
        return 0xFFFFFFFFU;
    }
    return (uint32_t)ms;
}

static void fill_simple_result(struct test_suite_result *out,
                               const char *name,
                               uint32_t total,
                               uint32_t passed,
                               uint32_t elapsed_ms) {
    if (!out) {
        return;
    }
    out->name = name;
    out->total = total;
    out->passed = passed;
    out->failed = (total > passed) ? (total - passed) : 0;
    out->exceptions_caught = 0;
    out->unexpected_exceptions = 0;
    out->elapsed_ms = elapsed_ms;
    out->timed_out = 0;
}

#if ENABLE_BUILTIN_TESTS
static int run_vm_suite(const struct interrupt_test_config *config,
                        struct test_suite_result *out) {
    (void)config;
    uint64_t start = cpu_read_tsc();
    int passed = run_vm_manager_tests();
    uint64_t end = cpu_read_tsc();
    fill_simple_result(out, "vm", 5, (uint32_t)passed, measure_elapsed_ms(start, end));
    return (passed == 5) ? 0 : -1;
}

static int run_heap_suite(const struct interrupt_test_config *config,
                          struct test_suite_result *out) {
    (void)config;
    uint64_t start = cpu_read_tsc();
    int passed = run_kernel_heap_tests();
    uint64_t end = cpu_read_tsc();
    fill_simple_result(out, "heap", 2, (uint32_t)passed, measure_elapsed_ms(start, end));
    return (passed == 2) ? 0 : -1;
}

static int run_ramfs_suite(const struct interrupt_test_config *config,
                           struct test_suite_result *out) {
    (void)config;
    uint64_t start = cpu_read_tsc();
    int passed = run_ramfs_tests();
    uint64_t end = cpu_read_tsc();
    fill_simple_result(out, "ramfs", 5, (uint32_t)passed, measure_elapsed_ms(start, end));
    return (passed == 5) ? 0 : -1;
}

static int run_privsep_suite(const struct interrupt_test_config *config,
                             struct test_suite_result *out) {
    (void)config;
    uint64_t start = cpu_read_tsc();
    int result = run_privilege_separation_invariant_test();
    uint64_t end = cpu_read_tsc();
    uint32_t passed = (result == 0) ? 1u : 0u;
    fill_simple_result(out, "privsep", 1, passed, measure_elapsed_ms(start, end));
    return result == 0 ? 0 : -1;
}

static int run_roulette_mapping_suite(const struct interrupt_test_config *config,
                                      struct test_suite_result *out) {
    (void)config;
    uint64_t start = cpu_read_tsc();

    const process_memory_layout_t *layout = mm_get_process_layout();
    uint64_t stack_probe = layout ? (layout->stack_top - 0x10) : 0;
    uint64_t heap_probe = mm_get_kernel_heap_start();

    uint32_t total = 3;
    uint32_t passed = 0;

    uint32_t pid = create_process_vm();
    if (pid != INVALID_PROCESS_ID) {
        process_page_dir_t *dir = process_vm_get_page_dir(pid);
        if (dir) {
            int code_ok = paging_is_user_accessible(dir, (uint64_t)roulette_user_main);
            int stack_ok = layout && paging_is_user_accessible(dir, stack_probe);
            int heap_guard = !paging_is_user_accessible(dir, heap_probe);
            passed = (code_ok ? 1u : 0u) + (stack_ok ? 1u : 0u) + (heap_guard ? 1u : 0u);
        }
        destroy_process_vm(pid);
    }

    uint64_t end = cpu_read_tsc();
    fill_simple_result(out, "roulette", total, passed, measure_elapsed_ms(start, end));
    return (passed == total) ? 0 : -1;
}

static int run_roulette_exec_suite(const struct interrupt_test_config *config,
                                   struct test_suite_result *out) {
    (void)config;
    uint64_t start = cpu_read_tsc();

    uint32_t total = 1;
    uint32_t passed = 0;

    uint32_t tid = task_create("roulette-test", roulette_user_main, NULL, 5, TASK_FLAG_USER_MODE);
    if (tid != INVALID_TASK_ID) {
        task_t *tinfo = NULL;
        if (task_get_info(tid, &tinfo) == 0) {
            if (schedule_task(tinfo) == 0 && tinfo->state == TASK_STATE_READY) {
                passed = 1;
            }
        }
        /* Clean up */
        task_terminate(tid);
    }

    uint64_t end = cpu_read_tsc();
    fill_simple_result(out, "roulette_exec", total, passed, measure_elapsed_ms(start, end));
    return (passed == total) ? 0 : -1;
}

#else
static int run_vm_suite(const struct interrupt_test_config *config,
                        struct test_suite_result *out) {
    (void)config;
    fill_simple_result(out, "vm", 0, 0, 0);
    return 0;
}

static int run_heap_suite(const struct interrupt_test_config *config,
                          struct test_suite_result *out) {
    (void)config;
    fill_simple_result(out, "heap", 0, 0, 0);
    return 0;
}

static int run_ramfs_suite(const struct interrupt_test_config *config,
                           struct test_suite_result *out) {
    (void)config;
    fill_simple_result(out, "ramfs", 0, 0, 0);
    return 0;
}

static int run_privsep_suite(const struct interrupt_test_config *config,
                             struct test_suite_result *out) {
    (void)config;
    fill_simple_result(out, "privsep", 0, 0, 0);
    return 0;
}

static int run_roulette_mapping_suite(const struct interrupt_test_config *config,
                                      struct test_suite_result *out) {
    (void)config;
    fill_simple_result(out, "roulette", 0, 0, 0);
    return 0;
}

static int run_roulette_exec_suite(const struct interrupt_test_config *config,
                                   struct test_suite_result *out) {
    (void)config;
    fill_simple_result(out, "roulette_exec", 0, 0, 0);
    return 0;
}

#endif

static const struct test_suite_desc vm_suite_desc = {
    .name = "vm",
    .mask_bit = INTERRUPT_TEST_SUITE_SCHEDULER,
    .run = run_vm_suite,
};

static const struct test_suite_desc heap_suite_desc = {
    .name = "heap",
    .mask_bit = INTERRUPT_TEST_SUITE_SCHEDULER,
    .run = run_heap_suite,
};

static const struct test_suite_desc ramfs_suite_desc = {
    .name = "ramfs",
    .mask_bit = INTERRUPT_TEST_SUITE_SCHEDULER,
    .run = run_ramfs_suite,
};

static const struct test_suite_desc privsep_suite_desc = {
    .name = "privsep",
    .mask_bit = INTERRUPT_TEST_SUITE_SCHEDULER,
    .run = run_privsep_suite,
};

static const struct test_suite_desc roulette_suite_desc = {
    .name = "roulette",
    .mask_bit = INTERRUPT_TEST_SUITE_SCHEDULER,
    .run = run_roulette_mapping_suite,
};

static const struct test_suite_desc roulette_exec_suite_desc = {
    .name = "roulette_exec",
    .mask_bit = INTERRUPT_TEST_SUITE_SCHEDULER,
    .run = run_roulette_exec_suite,
};

void tests_register_system_suites(void) {
    tests_register_suite(&vm_suite_desc);
    tests_register_suite(&heap_suite_desc);
    tests_register_suite(&ramfs_suite_desc);
    tests_register_suite(&privsep_suite_desc);
    tests_register_suite(&roulette_suite_desc);
    tests_register_suite(&roulette_exec_suite_desc);
}

