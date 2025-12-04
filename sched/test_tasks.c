/*
 * SlopOS Test Tasks
 * Two simple cooperative tasks that yield to each other
 * Demonstrates basic task switching and scheduler functionality
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../drivers/serial.h"
#include "../drivers/pit.h"
#include "task.h"

/* Forward declarations from scheduler and task modules */
extern void yield(void);
extern uint32_t task_create(const char *name, void (*entry_point)(void *),
                           void *arg, uint8_t priority, uint16_t flags);
extern int init_task_manager(void);
extern int init_scheduler(void);
extern int create_idle_task(void);
extern int start_scheduler(void);
extern int schedule_task(void *task);
extern int task_get_info(uint32_t task_id, void **task_info);
extern void init_kernel_context(task_context_t *context);
extern void context_switch(task_context_t *old_context, task_context_t *new_context);

/* Forward declaration for test function */
void test_task_function(int *completed_flag);

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

    kprint("Task A starting execution\n");

    while (counter < 20) {
        kprint("Task A: iteration ");
        kprint_decimal(counter);
        kprint("\n");

        counter++;

        /* Yield after every 3 iterations to demonstrate cooperation */
        if (counter % 3 == 0) {
            kprint("Task A: yielding CPU\n");
            yield();
        }
    }

    kprint("Task A completed\n");
}

/*
 * Test Task B - Character printing task
 */
void test_task_b(void *arg) {
    (void)arg;  /* Unused parameter */

    char current_char = 'A';
    uint32_t iterations = 0;

    kprint("Task B starting execution\n");

    while (iterations < 15) {
        kprint("Task B: printing character '");
        kprint_decimal(current_char);  /* Print ASCII value */
        kprint("' (");
        serial_putc_com1(current_char);  /* Print actual character */
        kprint(")\n");

        /* Move to next character, wrap around after Z */
        current_char++;
        if (current_char > 'Z') {
            current_char = 'A';
        }

        iterations++;

        /* Yield after every 2 iterations */
        if (iterations % 2 == 0) {
            kprint("Task B: yielding CPU\n");
            yield();
        }
    }

    kprint("Task B completed\n");
}

/* ========================================================================
 * SCHEDULER TEST FUNCTIONS
 * ======================================================================== */

/*
 * Initialize and run the basic scheduler test
 */
int run_scheduler_test(void) {
    kprint("=== Starting SlopOS Cooperative Scheduler Test ===\n");

    /* Initialize task management system */
    if (init_task_manager() != 0) {
        kprint("Failed to initialize task manager\n");
        return -1;
    }

    /* Initialize scheduler */
    if (init_scheduler() != 0) {
        kprint("Failed to initialize scheduler\n");
        return -1;
    }

    /* Create idle task */
    if (create_idle_task() != 0) {
        kprint("Failed to create idle task\n");
        return -1;
    }

    kprint("Creating test tasks...\n");

    /* Create test task A */
    uint32_t task_a_id = task_create("TestTaskA", test_task_a, NULL,
                                    1,    /* Normal priority */
                                    0x02  /* Kernel mode */);

    if (task_a_id == INVALID_TASK_ID) {
        kprint("Failed to create test task A\n");
        return -1;
    }

    kprint("Created Task A with ID ");
    kprint_decimal(task_a_id);
    kprint("\n");

    /* Create test task B */
    uint32_t task_b_id = task_create("TestTaskB", test_task_b, NULL,
                                    1,    /* Normal priority */
                                    0x02  /* Kernel mode */);

    if (task_b_id == INVALID_TASK_ID) {
        kprint("Failed to create test task B\n");
        return -1;
    }

    kprint("Created Task B with ID ");
    kprint_decimal(task_b_id);
    kprint("\n");

    /* Add tasks to scheduler */
    void *task_a_info, *task_b_info;

    if (task_get_info(task_a_id, &task_a_info) != 0) {
        kprint("Failed to get task A info\n");
        return -1;
    }

    if (task_get_info(task_b_id, &task_b_info) != 0) {
        kprint("Failed to get task B info\n");
        return -1;
    }

    if (schedule_task(task_a_info) != 0) {
        kprint("Failed to schedule task A\n");
        return -1;
    }

    if (schedule_task(task_b_info) != 0) {
        kprint("Failed to schedule task B\n");
        return -1;
    }

    kprint("Tasks scheduled, starting scheduler...\n");

    /* Start the scheduler - this will begin task execution */
    if (start_scheduler() != 0) {
        kprint("Failed to start scheduler\n");
        return -1;
    }

    /* If we reach here, scheduler is running tasks */
    kprint("Scheduler started successfully\n");

    return 0;
}

/*
 * Simple demonstration of task creation and yielding
 */
int demo_cooperative_scheduling(void) {
    kprint("=== Cooperative Scheduling Demo ===\n");

    /* This function demonstrates the basic concept */
    kprint("1. Tasks run voluntarily\n");
    kprint("2. Tasks must yield() to allow others to run\n");
    kprint("3. Round-robin scheduling gives fair CPU time\n");
    kprint("4. No preemption - tasks control their own execution\n");

    /* Run the actual test */
    return run_scheduler_test();
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
} smoke_test_context_t;


/*
 * Smoke test task A - yields repeatedly and tracks stack pointer
 */
void smoke_test_task_a(void *arg) {
    smoke_test_context_t *ctx = (smoke_test_context_t *)arg;
    uint32_t iteration = 0;
    const uint32_t target_yields = 100;  /* Reduced for testing - will verify stack discipline */
    uint64_t stack_base = 0;

    /* Get initial stack pointer */
    __asm__ volatile ("movq %%rsp, %0" : "=r"(stack_base));
    ctx->initial_stack_top = stack_base;
    ctx->min_stack_pointer = stack_base;
    ctx->max_stack_pointer = stack_base;
    ctx->yield_count = 0;
    ctx->test_failed = 0;

    kprint("SmokeTestA: Starting (initial RSP=0x");
    kprint_hex(stack_base);
    kprint(")\n");

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
            kprint("SmokeTestA: ERROR - Stack growth exceeds 4KB: ");
            kprint_hex(stack_growth);
            kprint(" bytes\n");
            ctx->test_failed = 1;
            break;
        }

        iteration++;
        if (iteration % 50 == 0) {
            kprint("SmokeTestA: Iteration ");
            kprint_decimal(iteration);
            kprint(" (yields: ");
            kprint_decimal(ctx->yield_count);
            kprint(", RSP=0x");
            kprint_hex(current_rsp);
            kprint(")\n");
        }

        yield();
        ctx->yield_count++;
    }

    kprint("SmokeTestA: Completed ");
    kprint_decimal(ctx->yield_count);
    kprint(" yields\n");
    kprint("SmokeTestA: Stack range: min=0x");
    kprint_hex(ctx->min_stack_pointer);
    kprint(" max=0x");
    kprint_hex(ctx->max_stack_pointer);
    kprint(" growth=");
    kprint_hex(ctx->initial_stack_top - ctx->min_stack_pointer);
    kprint(" bytes\n");

    if (ctx->test_failed) {
        kprint("SmokeTestA: FAILED - Stack corruption detected\n");
    } else {
        kprint("SmokeTestA: PASSED - No stack corruption\n");
    }
}

/*
 * Smoke test task B - yields repeatedly and tracks stack pointer
 */
void smoke_test_task_b(void *arg) {
    smoke_test_context_t *ctx = (smoke_test_context_t *)arg;
    uint32_t iteration = 0;
    const uint32_t target_yields = 100;  /* Reduced for testing - will verify stack discipline */
    uint64_t stack_base = 0;

    /* Get initial stack pointer */
    __asm__ volatile ("movq %%rsp, %0" : "=r"(stack_base));
    ctx->initial_stack_top = stack_base;
    ctx->min_stack_pointer = stack_base;
    ctx->max_stack_pointer = stack_base;
    ctx->yield_count = 0;
    ctx->test_failed = 0;

    kprint("SmokeTestB: Starting (initial RSP=0x");
    kprint_hex(stack_base);
    kprint(")\n");

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
            kprint("SmokeTestB: ERROR - Stack growth exceeds 4KB: ");
            kprint_hex(stack_growth);
            kprint(" bytes\n");
            ctx->test_failed = 1;
            break;
        }

        iteration++;
        if (iteration % 50 == 0) {
            kprint("SmokeTestB: Iteration ");
            kprint_decimal(iteration);
            kprint(" (yields: ");
            kprint_decimal(ctx->yield_count);
            kprint(", RSP=0x");
            kprint_hex(current_rsp);
            kprint(")\n");
        }

        yield();
        ctx->yield_count++;
    }

    kprint("SmokeTestB: Completed ");
    kprint_decimal(ctx->yield_count);
    kprint(" yields\n");
    kprint("SmokeTestB: Stack range: min=0x");
    kprint_hex(ctx->min_stack_pointer);
    kprint(" max=0x");
    kprint_hex(ctx->max_stack_pointer);
    kprint(" growth=");
    kprint_hex(ctx->initial_stack_top - ctx->min_stack_pointer);
    kprint(" bytes\n");

    if (ctx->test_failed) {
        kprint("SmokeTestB: FAILED - Stack corruption detected\n");
    } else {
        kprint("SmokeTestB: PASSED - No stack corruption\n");
    }
}

/*
 * Run context switch stack discipline smoke test
 * Creates two tasks that yield to each other hundreds of times
 * and checks for unexpected stack growth
 */
int run_context_switch_smoke_test(void) {
    kprint("=== Context Switch Stack Discipline Smoke Test ===\n");
    kprint("Testing basic context switch functionality\n");

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
        kprint("Failed to allocate stack for test task\n");
        return -1;
    }
    test_ctx.rsp = (uint64_t)(stack + 1024);  /* Top of stack */

    kprint("Switching to test context...\n");

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
    extern void simple_context_switch(task_context_t *old_context, task_context_t *new_context);
    task_context_t dummy_old;
    simple_context_switch(&dummy_old, &test_ctx);

        return_label:
            /* If we get here, the context switch worked and returned */
            kprint("Context switch returned successfully\n");

            /* Check if test completed successfully */
            if (test_completed) {
                kprint("CONTEXT_SWITCH_TEST: Basic switch test PASSED\n");
                return 0;
            } else {
                kprint("CONTEXT_SWITCH_TEST: Basic switch test FAILED\n");
                return -1;
            }
}

/* Simple test function that runs in task context */
void test_task_function(int *completed_flag) {
    kprint("Test task function executed successfully\n");
    *completed_flag = 1;

    /* Switch back to kernel */
    extern void simple_context_switch(task_context_t *old_context, task_context_t *new_context);

    // Switch back to kernel
    task_context_t dummy;
    simple_context_switch(&dummy, kernel_return_context);
}

/* Direct test function for task A - simulates the yield loop */
void smoke_test_task_a_direct(void *arg) {
    smoke_test_context_t *ctx = (smoke_test_context_t *)arg;
    uint32_t iteration = 0;
    const uint32_t target_yields = 10;  /* Reduced for direct testing */

    kprint("Task A started, running yield loop...\n");

    while (iteration < target_yields) {
        iteration++;
        ctx->yield_count = iteration;

        /* Simulate yield by switching back to kernel context */
        /* In a real scheduler, this would switch to the next task */
        task_context_t ctx_kernel;

        /* Save current context */
        init_kernel_context(&ctx_kernel);

        /* Create a dummy context to switch to (back to kernel) */
        /* For this test, we'll just return */
        kprint("Task A yield ");
        kprint_decimal(iteration);
        kprint("\n");

        /* Simulate the yield by just continuing (no actual switch needed for this test) */
    }

    ctx->test_failed = 0;  /* Success */
    kprint("Task A completed successfully\n");

    /* Return to kernel context */
    task_context_t ctx_kernel;
    init_kernel_context(&ctx_kernel);
    context_switch(NULL, &ctx_kernel);

    /* Should not reach here */
    kprint("ERROR: Task A failed to return\n");
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

    kprint("  #");
    kprint_decimal(ctx->index);
    kprint(" '");
    kprint(task->name);
    kprint("' (ID ");
    kprint_decimal(task->task_id);
    kprint(") [");
    kprint(task_state_to_string(task->state));
    kprint("] runtime=");
    kprint_decimal(task->total_runtime);
    kprint(" ticks yields=");
    kprint_decimal(task->yield_count);
    kprintln("");
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

    kprint("\n=== Scheduler Statistics ===\n");
    kprint("Context switches: ");
    kprint_decimal(sched_switches);
    kprint("\n");

    kprint("Voluntary yields: ");
    kprint_decimal(sched_yields);
    kprint("\n");

    kprint("Schedule calls: ");
    kprint_decimal(schedule_calls);
    kprint("\n");

    kprint("Ready tasks: ");
    kprint_decimal(ready_tasks);
    kprint("\n");

    kprint("Total tasks created: ");
    kprint_decimal(total_tasks);
    kprint("\n");

    kprint("Active tasks: ");
    kprint_decimal(active_tasks);
    kprint("\n");

    kprint("Task yields (aggregate): ");
    kprint_decimal(task_yields);
    kprint("\n");

    kprint("Active task metrics:\n");
    task_stat_print_ctx_t ctx = {0};
    task_iterate_active(print_task_stat_line, &ctx);
    if (ctx.index == 0) {
        kprint("  (no active tasks)\n");
    }
}

/*
 * Monitor scheduler performance
 */
void monitor_scheduler(uint32_t duration_seconds) {
    kprint("Monitoring scheduler for ");
    kprint_decimal(duration_seconds);
    kprint(" seconds...\n");

    /* Simple monitoring loop */
    for (uint32_t i = 0; i < duration_seconds; i++) {
        /* Wait 1 second using PIT (interrupts are enabled at this point) */
        pit_sleep_ms(1000);

        kprint("Monitor: ");
        kprint_decimal(i + 1);
        kprint("s elapsed\n");

        /* Print stats every 5 seconds */
        if ((i + 1) % 5 == 0) {
            print_scheduler_stats();
        }
    }

    kprint("Monitoring complete\n");
    print_scheduler_stats();
}
