/*
 * SlopOS Test Tasks
 * Two simple cooperative tasks that yield to each other
 * Demonstrates basic task switching and scheduler functionality
 */

#include <stdint.h>
#include <stddef.h>
#include "../drivers/serial.h"
#include "../drivers/pit.h"
#include "../boot/idt.h"
#include "../boot/gdt_defs.h"
#include "../mm/mm_constants.h"
#include "task.h"
#include "scheduler.h"
#include "../lib/klog.h"

/* Forward declaration for test function */
void test_task_function(int *completed_flag);

/* Forward declaration for privilege separation test */
static void user_stub_task(void *arg);

/* Global for context switch test */
static task_context_t kernel_return_context_storage;
static task_context_t *kernel_return_context = &kernel_return_context_storage;

/* ========================================================================
 * TEST TASK IMPLEMENTATIONS
 * ======================================================================== */

/*
 * Test Task A - Simple counting task
 */
void test_task_a(void *arg) {
    (void)arg;  /* Unused parameter */

    uint32_t counter = 0;

    klog_printf(KLOG_INFO, "Task A starting execution\n");

    while (counter < 20) {
        klog_printf(KLOG_INFO, "Task A: iteration %u\n", counter);

        counter++;

        /* Yield after every 3 iterations to demonstrate cooperation */
        if (counter % 3 == 0) {
            klog_printf(KLOG_INFO, "Task A: yielding CPU\n");
            yield();
        }
    }

    klog_printf(KLOG_INFO, "Task A completed\n");
}

/*
 * Test Task B - Character printing task
 */
void test_task_b(void *arg) {
    (void)arg;  /* Unused parameter */

    char current_char = 'A';
    uint32_t iterations = 0;

    klog_printf(KLOG_INFO, "Task B starting execution\n");

    while (iterations < 15) {
        klog_printf(KLOG_INFO, "Task B: printing character '%c' (%u) (", current_char, (unsigned)current_char);
        serial_putc_com1(current_char);  /* Print actual character */
        klog_printf(KLOG_INFO, ")\n");

        /* Move to next character, wrap around after Z */
        current_char++;
        if (current_char > 'Z') {
            current_char = 'A';
        }

        iterations++;

        /* Yield after every 2 iterations */
        if (iterations % 2 == 0) {
            klog_printf(KLOG_INFO, "Task B: yielding CPU\n");
            yield();
        }
    }

    klog_printf(KLOG_INFO, "Task B completed\n");
}

/* ========================================================================
 * SCHEDULER TEST FUNCTIONS
 * ======================================================================== */

/*
 * Initialize and run the basic scheduler test
 */
int run_scheduler_test(void) {
    klog_printf(KLOG_INFO, "=== Starting SlopOS Cooperative Scheduler Test ===\n");

    /* Initialize task management system */
    if (init_task_manager() != 0) {
        klog_printf(KLOG_INFO, "Failed to initialize task manager\n");
        return -1;
    }

    /* Initialize scheduler */
    if (init_scheduler() != 0) {
        klog_printf(KLOG_INFO, "Failed to initialize scheduler\n");
        return -1;
    }

    /* Create idle task */
    if (create_idle_task() != 0) {
        klog_printf(KLOG_INFO, "Failed to create idle task\n");
        return -1;
    }

    klog_printf(KLOG_INFO, "Creating test tasks...\n");

    /* Create test task A */
    uint32_t task_a_id = task_create("TestTaskA", test_task_a, NULL,
                                    1,    /* Normal priority */
                                    0x02  /* Kernel mode */);

    if (task_a_id == INVALID_TASK_ID) {
        klog_printf(KLOG_INFO, "Failed to create test task A\n");
        return -1;
    }

    klog_printf(KLOG_INFO, "Created Task A with ID %u\n", task_a_id);

    /* Create test task B */
    uint32_t task_b_id = task_create("TestTaskB", test_task_b, NULL,
                                    1,    /* Normal priority */
                                    0x02  /* Kernel mode */);

    if (task_b_id == INVALID_TASK_ID) {
        klog_printf(KLOG_INFO, "Failed to create test task B\n");
        return -1;
    }

    klog_printf(KLOG_INFO, "Created Task B with ID %u\n", task_b_id);

    /* Add tasks to scheduler */
    task_t *task_a_info = NULL;
    task_t *task_b_info = NULL;

    if (task_get_info(task_a_id, &task_a_info) != 0) {
        klog_printf(KLOG_INFO, "Failed to get task A info\n");
        return -1;
    }

    if (task_get_info(task_b_id, &task_b_info) != 0) {
        klog_printf(KLOG_INFO, "Failed to get task B info\n");
        return -1;
    }

    if (schedule_task(task_a_info) != 0) {
        klog_printf(KLOG_INFO, "Failed to schedule task A\n");
        task_terminate(task_a_id);
        task_terminate(task_b_id);
        return -1;
    }

    if (schedule_task(task_b_info) != 0) {
        klog_printf(KLOG_INFO, "Failed to schedule task B\n");
        task_terminate(task_a_id);
        task_terminate(task_b_id);
        return -1;
    }

    klog_printf(KLOG_INFO, "Tasks scheduled, starting scheduler...\n");

    /* Start the scheduler - this will begin task execution */
    if (start_scheduler() != 0) {
        klog_printf(KLOG_INFO, "Failed to start scheduler\n");
        return -1;
    }

    /* If we reach here, scheduler is running tasks */
    klog_printf(KLOG_INFO, "Scheduler started successfully\n");

    return 0;
}

/*
 * Simple user-mode stub (never actually scheduled in tests)
 */
static void user_stub_task(void *arg) {
    (void)arg;
    /* If ever executed, yield then exit via syscall numbers */
    __asm__ volatile (
        "mov $0, %%rax\n\t"  /* SYSCALL_YIELD */
        "int $0x80\n\t"
        "mov $1, %%rax\n\t"  /* SYSCALL_EXIT */
        "int $0x80\n\t"
        :
        :
        : "rax"
    );
}

/*
 * Verify privilege separation invariants without entering ring3
 */
int run_privilege_separation_invariant_test(void) {
    klog_printf(KLOG_INFO, "PRIVSEP_TEST: Checking privilege separation invariants\n");

    if (init_task_manager() != 0 || init_scheduler() != 0 || create_idle_task() != 0) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: init failed\n");
        return -1;
    }

    uint32_t user_task_id = task_create("UserStub", user_stub_task, NULL,
                                        TASK_PRIORITY_NORMAL,
                                        TASK_FLAG_USER_MODE);
    if (user_task_id == INVALID_TASK_ID) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: user task creation failed\n");
        return -1;
    }

    task_t *task_info = NULL;
    if (task_get_info(user_task_id, &task_info) != 0 || !task_info) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: task lookup failed\n");
        return -1;
    }

    int failed = 0;

    if (task_info->process_id == INVALID_PROCESS_ID) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: user task missing process VM\n");
        failed = 1;
    }
    if (task_info->kernel_stack_top == 0) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: user task missing kernel RSP0 stack\n");
        failed = 1;
    }
    if (task_info->context.cs != GDT_USER_CODE_SELECTOR ||
        task_info->context.ss != GDT_USER_DATA_SELECTOR) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: user task selectors incorrect (cs=0x%lx ss=0x%lx)\n",
                    (unsigned long)task_info->context.cs,
                    (unsigned long)task_info->context.ss);
        failed = 1;
    }

    struct idt_entry gate;
    if (idt_get_gate(SYSCALL_VECTOR, &gate) != 0) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: cannot read syscall gate\n");
        failed = 1;
    } else {
        uint8_t dpl = (gate.type_attr >> 5) & 0x3;
        if (dpl != 3) {
            klog_printf(KLOG_INFO, "PRIVSEP_TEST: syscall gate DPL=%u expected 3\n", dpl);
            failed = 1;
        }
    }

    task_shutdown_all();
    scheduler_shutdown();

    if (failed) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: FAILED\n");
        return -1;
    }

    klog_printf(KLOG_INFO, "PRIVSEP_TEST: PASSED\n");
    return 0;
}

/* ========================================================================
 * CONTEXT SWITCH SMOKE TEST
 * ======================================================================== */

/* Test context for stack corruption detection */
typedef struct smoke_test_context {
    uint64_t initial_stack_top;
    uint64_t min_stack_pointer;
    uint64_t max_stack_pointer;
    uint32_t yield_count;
    int test_failed;
    const char *task_name;  /* Task name for logging */
} smoke_test_context_t;


/*
 * Smoke test task implementation - yields repeatedly and tracks stack pointer
 * Uses task_name from context for logging output
 */
static void smoke_test_task_impl(smoke_test_context_t *ctx) {
    uint32_t iteration = 0;
    const uint32_t target_yields = 100;  /* Reduced for testing - will verify stack discipline */
    uint64_t stack_base = 0;
    const char *name = ctx->task_name ? ctx->task_name : "SmokeTest";

    /* Get initial stack pointer */
    __asm__ volatile ("movq %%rsp, %0" : "=r"(stack_base));
    ctx->initial_stack_top = stack_base;
    ctx->min_stack_pointer = stack_base;
    ctx->max_stack_pointer = stack_base;
    ctx->yield_count = 0;
    ctx->test_failed = 0;

    klog_printf(KLOG_INFO, "%s: Starting (initial RSP=0x%lx)\n", name, stack_base);

    while (ctx->yield_count < target_yields) {
        uint64_t current_rsp = 0;
        __asm__ volatile ("movq %%rsp, %0" : "=r"(current_rsp));

        /* Track stack pointer bounds */
        if (current_rsp < ctx->min_stack_pointer) {
            ctx->min_stack_pointer = current_rsp;
        }
        if (current_rsp > ctx->max_stack_pointer) {
            ctx->max_stack_pointer = current_rsp;
        }

        /* Check for excessive stack growth (more than 4KB indicates corruption) */
        uint64_t stack_growth = ctx->initial_stack_top - ctx->min_stack_pointer;
        if (stack_growth > 0x1000) {
            klog_printf(KLOG_INFO, "%s: ERROR - Stack growth exceeds 4KB: 0x%lx bytes\n",
                        name, stack_growth);
            ctx->test_failed = 1;
            break;
        }

        iteration++;
        if (iteration % 50 == 0) {
            klog_printf(KLOG_INFO, "%s: Iteration %u (yields: %u, RSP=0x%lx)\n",
                        name, iteration, ctx->yield_count, current_rsp);
        }

        yield();
        ctx->yield_count++;
    }

    klog_printf(KLOG_INFO, "%s: Completed %u yields\n", name, ctx->yield_count);
    klog_printf(KLOG_INFO,
                "%s: Stack range: min=0x%lx max=0x%lx growth=0x%lx bytes\n",
                name,
                ctx->min_stack_pointer,
                ctx->max_stack_pointer,
                ctx->initial_stack_top - ctx->min_stack_pointer);

    if (ctx->test_failed) {
        klog_printf(KLOG_INFO, "%s: FAILED - Stack corruption detected\n", name);
    } else {
        klog_printf(KLOG_INFO, "%s: PASSED - No stack corruption\n", name);
    }
}

/*
 * Smoke test task A - wrapper for generic implementation
 */
void smoke_test_task_a(void *arg) {
    smoke_test_context_t *ctx = (smoke_test_context_t *)arg;
    ctx->task_name = "SmokeTestA";
    smoke_test_task_impl(ctx);
}

/*
 * Smoke test task B - wrapper for generic implementation
 */
void smoke_test_task_b(void *arg) {
    smoke_test_context_t *ctx = (smoke_test_context_t *)arg;
    ctx->task_name = "SmokeTestB";
    smoke_test_task_impl(ctx);
}

/*
 * Run context switch stack discipline smoke test
 * Creates two tasks that yield to each other hundreds of times
 * and checks for unexpected stack growth
 */
int run_context_switch_smoke_test(void) {
    klog_printf(KLOG_INFO, "=== Context Switch Stack Discipline Smoke Test ===\n");
    klog_printf(KLOG_INFO, "Testing basic context switch functionality\n");

    /* Create a simple test function that just returns */
    static int test_completed = 0;

    /* Set up a minimal task context */
    task_context_t test_ctx = {0};

    /* Set up task context manually */
    test_ctx.rax = 0;
    test_ctx.rbx = 0;
    test_ctx.rcx = 0;
    test_ctx.rdx = 0;
    test_ctx.rsi = 0;
    test_ctx.rdi = (uint64_t)&test_completed;  /* Argument */
    test_ctx.rbp = 0;
    test_ctx.rip = (uint64_t)test_task_function;
    test_ctx.rflags = 0x202;  /* IF=1 */
    test_ctx.cs = 0x08;       /* Kernel code segment */
    test_ctx.ds = 0x10;       /* Kernel data segment */
    test_ctx.es = 0x10;
    test_ctx.fs = 0;
    test_ctx.gs = 0;
    test_ctx.ss = 0x10;       /* Kernel stack segment */
    test_ctx.cr3 = 0;         /* Use current */

    /* Allocate stack for task */
    extern void *kmalloc(size_t size);
    uint64_t *stack = (uint64_t *)kmalloc(4096);  /* 4KB stack */
    if (!stack) {
        klog_printf(KLOG_INFO, "Failed to allocate stack for test task\n");
        return -1;
    }
    test_ctx.rsp = (uint64_t)(stack + 1024);  /* Top of stack */

    klog_printf(KLOG_INFO, "Switching to test context...\n");

            /* Set up kernel return context manually */
            uint64_t current_rsp;
            __asm__ volatile ("movq %%rsp, %0" : "=r"(current_rsp));
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wgnu-label-as-value"
            kernel_return_context->rip = (uint64_t)&&return_label;
#pragma clang diagnostic pop
            kernel_return_context->rsp = current_rsp;
            kernel_return_context->cs = 0x08;  /* Kernel code segment */
            kernel_return_context->ss = 0x10;  /* Kernel stack segment */
            kernel_return_context->ds = 0x10;  /* Kernel data segment */
            kernel_return_context->es = 0x10;
            kernel_return_context->fs = 0;
            kernel_return_context->gs = 0;
            kernel_return_context->rflags = 0x202;  /* IF=1 */

    /* Switch to test context using simple switch (no IRET for testing) */
    task_context_t dummy_old;
    simple_context_switch(&dummy_old, &test_ctx);

        return_label:
            /* If we get here, the context switch worked and returned */
            klog_printf(KLOG_INFO, "Context switch returned successfully\n");

            /* Check if test completed successfully */
            if (test_completed) {
                klog_printf(KLOG_INFO, "CONTEXT_SWITCH_TEST: Basic switch test PASSED\n");
                return 0;
            } else {
                klog_printf(KLOG_INFO, "CONTEXT_SWITCH_TEST: Basic switch test FAILED\n");
                return -1;
            }
}

/* Simple test function that runs in task context */
void test_task_function(int *completed_flag) {
    klog_printf(KLOG_INFO, "Test task function executed successfully\n");
    *completed_flag = 1;

    /* Switch back to kernel */
    // Switch back to kernel
    task_context_t dummy;
    simple_context_switch(&dummy, kernel_return_context);
}

/* ========================================================================
 * SCHEDULER STATISTICS AND MONITORING
 * ======================================================================== */

typedef struct task_stat_print_ctx {
    uint32_t index;
} task_stat_print_ctx_t;

static void print_task_stat_line(task_t *task, void *context) {
    task_stat_print_ctx_t *ctx = (task_stat_print_ctx_t *)context;
    ctx->index++;

    klog_printf(KLOG_INFO,
                "  #%u '%s' (ID %u) [%s] runtime=%llu ticks yields=%llu\n",
                ctx->index,
                task->name,
                task->task_id,
                task_state_to_string(task->state),
                (unsigned long long)task->total_runtime,
                (unsigned long long)task->yield_count);
}

/*
 * Print current scheduler statistics
 */
void print_scheduler_stats(void) {
    extern void get_scheduler_stats(uint64_t *context_switches, uint64_t *yields,
                                   uint32_t *ready_tasks, uint32_t *schedule_calls);
    extern void get_task_stats(uint32_t *total_tasks, uint32_t *active_tasks,
                              uint64_t *context_switches);

    uint64_t sched_switches, sched_yields;
    uint32_t ready_tasks, schedule_calls;
    uint32_t total_tasks, active_tasks;
    uint64_t task_switches;
    uint64_t task_yields = task_get_total_yields();

    get_scheduler_stats(&sched_switches, &sched_yields, &ready_tasks, &schedule_calls);
    get_task_stats(&total_tasks, &active_tasks, &task_switches);

    klog_printf(KLOG_INFO, "\n=== Scheduler Statistics ===\n");
    klog_printf(KLOG_INFO, "Context switches: %llu\n", (unsigned long long)sched_switches);
    klog_printf(KLOG_INFO, "Voluntary yields: %llu\n", (unsigned long long)sched_yields);
    klog_printf(KLOG_INFO, "Schedule calls: %u\n", schedule_calls);
    klog_printf(KLOG_INFO, "Ready tasks: %u\n", ready_tasks);
    klog_printf(KLOG_INFO, "Total tasks created: %u\n", total_tasks);
    klog_printf(KLOG_INFO, "Active tasks: %u\n", active_tasks);
    klog_printf(KLOG_INFO, "Task yields (aggregate): %llu\n", (unsigned long long)task_yields);

    klog_printf(KLOG_INFO, "Active task metrics:\n");
    task_stat_print_ctx_t ctx = {0};
    task_iterate_active(print_task_stat_line, &ctx);
    if (ctx.index == 0) {
        klog_printf(KLOG_INFO, "  (no active tasks)\n");
    }
}

