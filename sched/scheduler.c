/*
 * SlopOS Cooperative Round-Robin Scheduler
 * Implements fair task scheduling with voluntary yielding.
 * Preemption is opt-in; default mode is cooperative.
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/init.h"
#include "../lib/kdiag.h"
#include "../lib/klog.h"
#include "../drivers/pit.h"
#include "../drivers/wl_currency.h"
#include "../mm/paging.h"
#include "../mm/process_vm.h"
#include "../boot/gdt.h"
#include "../lib/spinlock.h"
#include "scheduler.h"

/* ========================================================================
 * SCHEDULER CONSTANTS
 * ======================================================================== */

#define SCHED_DEFAULT_TIME_SLICE      10        /* Default time slice units */
#define SCHED_IDLE_TASK_ID            0xFFFFFFFE /* Special idle task ID */

/* Scheduling policies */
#define SCHED_POLICY_ROUND_ROBIN      0         /* Round-robin scheduling */
#define SCHED_POLICY_PRIORITY         1         /* Priority-based scheduling */
#define SCHED_POLICY_COOPERATIVE      2         /* Pure cooperative scheduling */

/* ========================================================================
 * SCHEDULER DATA STRUCTURES
 * ======================================================================== */

/* Ready queue for runnable tasks */
typedef struct ready_queue {
    task_t *head;                         /* First runnable task */
    task_t *tail;                         /* Last runnable task */
    uint32_t count;                       /* Number of tasks in queue */
} ready_queue_t;

/* Scheduler control structure */
typedef struct scheduler {
    ready_queue_t ready_queue;             /* Queue of ready tasks */
    task_t *current_task;                  /* Currently running task */
    task_t *idle_task;                     /* Idle task (always ready) */

    /* Scheduling policy and configuration */
    uint8_t policy;                        /* Current scheduling policy */
    uint8_t enabled;                       /* Scheduler enabled flag */
    uint16_t time_slice;                   /* Current time slice value */

    /* Return context for testing (when scheduler exits) */
    task_context_t return_context;         /* Context to return to when scheduler exits */

    /* Statistics and monitoring */
    uint64_t total_switches;               /* Total context switches */
    uint64_t total_yields;                 /* Total voluntary yields */
    uint64_t idle_time;                    /* Time spent in idle task */
    uint64_t total_ticks;                  /* Timer ticks observed */
    uint64_t total_preemptions;            /* Forced preemptions */
    uint32_t schedule_calls;               /* Number of schedule() calls */
    uint8_t preemption_enabled;            /* Preemption toggle */
    uint8_t reschedule_pending;            /* Deferred reschedule request */
    uint8_t in_schedule;                   /* Recursion guard */
    uint8_t reserved;                      /* Padding */
} scheduler_t;

/* Global scheduler instance */
static scheduler_t scheduler = {0};
static scheduler_idle_wakeup_cb_t idle_wakeup_cb = NULL;
static const uint8_t scheduler_preemption_default = 1; /* enable PIT IRQ preemption by default */
static spinlock_t scheduler_lock;

static inline uint64_t sched_lock(void) {
    return spinlock_lock_irqsave(&scheduler_lock);
}

static inline void sched_unlock(uint64_t guard) {
    spinlock_unlock_irqrestore(&scheduler_lock, guard);
}

static uint32_t scheduler_get_default_time_slice(void) {
    return scheduler.time_slice ? scheduler.time_slice : SCHED_DEFAULT_TIME_SLICE;
}

static void scheduler_reset_task_quantum(task_t *task) {
    if (!task) {
        return;
    }

    uint64_t slice = task->time_slice ? task->time_slice : scheduler_get_default_time_slice();
    task->time_slice = slice;
    task->time_slice_remaining = slice;
}

/* ========================================================================
 * READY QUEUE MANAGEMENT
 * ======================================================================== */

/*
 * Initialize the ready queue
 */
static void ready_queue_init(ready_queue_t *queue) {
    queue->head = NULL;
    queue->tail = NULL;
    queue->count = 0;
}

/*
 * Check if ready queue is empty
 */
static int ready_queue_empty(ready_queue_t *queue) {
    return queue->count == 0;
}

static int ready_queue_contains(ready_queue_t *queue, task_t *task) {
    for (task_t *cursor = queue->head; cursor; cursor = cursor->next_ready) {
        if (cursor == task) {
            return 1;
        }
    }
    return 0;
}

/*
 * Add task to ready queue
 * Returns 0 on success, -1 if queue is full
 */
static int ready_queue_enqueue(ready_queue_t *queue, task_t *task) {
    if (!task) {
        return -1;
    }

    /* Avoid double-enqueue to keep list well-formed. */
    if (ready_queue_contains(queue, task)) {
        return 0;
    }

    task->next_ready = NULL;
    if (!queue->head) {
        queue->head = queue->tail = task;
    } else {
        queue->tail->next_ready = task;
        queue->tail = task;
    }
    queue->count++;
    return 0;
}

/*
 * Remove task from front of ready queue
 * Returns task pointer, NULL if queue is empty
 */
static task_t *ready_queue_dequeue(ready_queue_t *queue) {
    if (ready_queue_empty(queue)) {
        return NULL;
    }

    task_t *task = queue->head;
    queue->head = task->next_ready;
    if (!queue->head) {
        queue->tail = NULL;
    }
    task->next_ready = NULL;
    queue->count--;
    return task;
}

/*
 * Remove specific task from ready queue
 * Returns 0 on success, -1 if task not found
 */
static int ready_queue_remove(ready_queue_t *queue, task_t *task) {
    if (!task || ready_queue_empty(queue)) {
        return -1;
    }

    task_t *prev = NULL;
    task_t *cursor = queue->head;
    while (cursor) {
        if (cursor == task) {
            if (prev) {
                prev->next_ready = cursor->next_ready;
            } else {
                queue->head = cursor->next_ready;
            }
            if (queue->tail == cursor) {
                queue->tail = prev;
            }
            cursor->next_ready = NULL;
            queue->count--;
            return 0;
        }
        prev = cursor;
        cursor = cursor->next_ready;
    }

    return -1;  /* Task not found */
}

/* ========================================================================
 * CORE SCHEDULING FUNCTIONS
 * ======================================================================== */

/*
 * Add task to ready queue for scheduling
 */
int schedule_task(task_t *task) {
    if (!task) {
        return -1;
    }

    uint64_t guard = sched_lock();
    if (!task_is_ready(task)) {
        klog_printf(KLOG_INFO, "schedule_task: task %u not ready (state %s)\n",
                    task->task_id, task_state_to_string(task_get_state(task)));
        sched_unlock(guard);
        return -1;
    }

    if (task->time_slice_remaining == 0) {
        scheduler_reset_task_quantum(task);
    }

    if (ready_queue_enqueue(&scheduler.ready_queue, task) != 0) {
        klog_printf(KLOG_INFO, "schedule_task: ready queue full, request rejected\n");
        wl_award_loss();
        sched_unlock(guard);
        return -1;
    }

    sched_unlock(guard);
    return 0;
}

/*
 * Remove task from scheduler (task blocked or terminated)
 */
int unschedule_task(task_t *task) {
    if (!task) {
        return -1;
    }

    uint64_t guard = sched_lock();
    /* Remove from ready queue if present */
    ready_queue_remove(&scheduler.ready_queue, task);

    /* If this was the current task, mark for rescheduling */
    if (scheduler.current_task == task) {
        scheduler.current_task = NULL;
    }

    sched_unlock(guard);
    return 0;
}

/*
 * Select next task to run using round-robin policy
 */
static task_t *select_next_task(void) {
    task_t *next_task = NULL;

    /* Get next task from ready queue */
    if (!ready_queue_empty(&scheduler.ready_queue)) {
        next_task = ready_queue_dequeue(&scheduler.ready_queue);
    }

    /* If no tasks available, use idle task */
    if (!next_task && scheduler.idle_task && !task_is_terminated(scheduler.idle_task)) {
        next_task = scheduler.idle_task;
    }

    return next_task;
}

/*
 * Perform context switch to new task
 */
static void switch_to_task(task_t *new_task) {
    if (!new_task) {
        return;
    }

    task_t *old_task = scheduler.current_task;
    task_context_t *old_ctx_ptr = NULL;

    if (old_task == new_task) {
        return;
    }

    uint64_t timestamp = kdiag_timestamp();
    task_record_context_switch(old_task, new_task, timestamp);

    /* Update scheduler state */
    scheduler.current_task = new_task;
    task_set_current(new_task);
    scheduler_reset_task_quantum(new_task);
    scheduler.total_switches++;

    /* Preserve old context unless it was captured from user mode already */
    if (old_task && !old_task->context_from_user) {
        old_ctx_ptr = &old_task->context;
    } else if (old_task) {
        old_task->context_from_user = 0;
    }

    /* Ensure CR3 matches the task's process address space */
    if (new_task->process_id != INVALID_PROCESS_ID) {
        process_page_dir_t *page_dir = process_vm_get_page_dir(new_task->process_id);
        if (page_dir && page_dir->pml4_phys) {
            new_task->context.cr3 = page_dir->pml4_phys;
            paging_set_current_directory(page_dir);
        }
    } else {
        paging_set_current_directory(paging_get_kernel_directory());
    }

    /* Check W/L balance before switching - user must not be bankrupt */
    wl_check_balance();

    /*
     * PRIVILEGE-AWARE CONTEXT SWITCHING:
     * 
     * User mode tasks (TASK_FLAG_USER_MODE):
     *  1. Update TSS.RSP0 to point to the task's kernel stack
     *     - This stack will be used when the task triggers a syscall or exception
     *     - The CPU automatically switches to RSP0 on Ring 3 → Ring 0 transitions
     *  2. Call context_switch_user() which uses IRETQ to enter Ring 3
     *     - The task's CS/SS are set to user selectors (0x23/0x1B, DPL=3, RPL=3)
     *     - The task executes with CPL=3 (Current Privilege Level)
     *     - Memory accesses are validated against U/S bits in page tables
     *
     * Kernel mode tasks (TASK_FLAG_KERNEL_MODE):
     *  1. Set RSP0 to the default kernel stack (not used since we stay in Ring 0)
     *  2. Use context_switch() which performs a simple JMP to the new RIP
     *     - No privilege change occurs (stays at CPL=0)
     *     - CS/SS remain at kernel selectors (0x08/0x10)
     *     - Full access to kernel memory
     *
     * Security note: The TSS.RSP0 update MUST occur before entering user mode,
     * otherwise the next interrupt/syscall will use an invalid kernel stack,
     * leading to a triple fault or privilege escalation vulnerability.
     */
    if (new_task->flags & TASK_FLAG_USER_MODE) {
        uint64_t rsp0 = new_task->kernel_stack_top ? new_task->kernel_stack_top : (uint64_t)&kernel_stack_top;
        gdt_set_kernel_rsp0(rsp0);
        context_switch_user(old_ctx_ptr, &new_task->context);
    } else {
        gdt_set_kernel_rsp0((uint64_t)&kernel_stack_top);
        if (old_ctx_ptr) {
            context_switch(old_ctx_ptr, &new_task->context);
        } else {
            /* First task or user-context-saved switch */
            context_switch(NULL, &new_task->context);
        }
    }
}

/* ========================================================================
 * PUBLIC SCHEDULER INTERFACE
 * ======================================================================== */

/*
 * Main scheduling function - select and switch to next task
 * This is the core of the cooperative scheduler
 */
void schedule(void) {
    if (!scheduler.enabled) {
        return;
    }

    uint64_t guard = sched_lock();
    scheduler.in_schedule++;
    scheduler.schedule_calls++;

    /* Get current task and put it back in ready queue if still runnable */
    task_t *current = scheduler.current_task;
    if (current && current != scheduler.idle_task) {
        if (task_is_running(current)) {
            if (task_set_state(current->task_id, TASK_STATE_READY) != 0) {
                klog_printf(KLOG_INFO, "schedule: failed to mark task %u ready\n", current->task_id);
            } else if (ready_queue_enqueue(&scheduler.ready_queue, current) != 0) {
                klog_printf(KLOG_INFO, "schedule: ready queue full when re-queuing task %u\n",
                            current->task_id);
                /* Backpressure: keep running the current task instead of dropping it. */
                task_set_state(current->task_id, TASK_STATE_RUNNING);
                scheduler_reset_task_quantum(current);
                scheduler.in_schedule--;
                sched_unlock(guard);
                return;
            } else {
                scheduler_reset_task_quantum(current);
            }
        } else if (!task_is_blocked(current) && !task_is_terminated(current)) {
            klog_printf(KLOG_INFO, "schedule: skipping requeue for task %u in state %s\n",
                        current->task_id, task_state_to_string(task_get_state(current)));
        }
    }

    /* Select next task to run */
    task_t *next_task = select_next_task();
    if (!next_task) {
        /* No tasks to run - check if we should exit scheduler */
        /* For testing purposes, if idle task has terminated, exit scheduler */
        if (scheduler.idle_task && task_is_terminated(scheduler.idle_task)) {
            /* Idle task terminated - exit scheduler by switching to return context */
            scheduler.enabled = 0;
            /* Switch back to the saved return context */
            if (scheduler.current_task) {
                scheduler.in_schedule--;
                sched_unlock(guard);
                context_switch(&scheduler.current_task->context, &scheduler.return_context);
                return;
            } else {
                /* No current task - this shouldn't happen */
                goto out;
            }
        }
        /* No tasks available but idle task still exists - shouldn't happen */
        goto out;
    }

    /* Switch to the selected task */
    scheduler.in_schedule--;
    sched_unlock(guard);
    switch_to_task(next_task);
    return;

out:
    if (scheduler.in_schedule > 0) {
        scheduler.in_schedule--;
    }
    sched_unlock(guard);
}

/*
 * Yield CPU voluntarily (cooperative scheduling)
 * Current task gives up CPU and allows other tasks to run
 */
void yield(void) {
    scheduler.total_yields++;

    if (scheduler.current_task) {
        task_record_yield(scheduler.current_task);
    }

    /* Trigger rescheduling */
    schedule();
}

/*
 * Block current task (remove from ready queue)
 */
void block_current_task(void) {
    task_t *current = scheduler.current_task;
    if (!current) {
        return;
    }

    /* Mark task as blocked */
    if (task_set_state(current->task_id, TASK_STATE_BLOCKED) != 0) {
        klog_printf(KLOG_INFO, "block_current_task: invalid state transition for task %u\n",
                    current->task_id);
    }

    /* Remove from ready queue and schedule next task */
    unschedule_task(current);
    schedule();
}

int task_wait_for(uint32_t task_id) {
    task_t *current = scheduler.current_task;
    if (!current) {
        return -1;
    }

    if (task_id == INVALID_TASK_ID || current->task_id == task_id) {
        return -1;
    }

    task_t *target = NULL;
    if (task_get_info(task_id, &target) != 0 || !target) {
        current->waiting_on_task_id = INVALID_TASK_ID;
        return 0; /* Target already gone */
    }

    if (target->state == TASK_STATE_INVALID || target->task_id == INVALID_TASK_ID) {
        current->waiting_on_task_id = INVALID_TASK_ID;
        return 0;
    }

    current->waiting_on_task_id = task_id;
    block_current_task();

    current->waiting_on_task_id = INVALID_TASK_ID;
    return 0;
}

/*
 * Unblock task (add back to ready queue)
 */
int unblock_task(task_t *task) {
    if (!task) {
        return -1;
    }

    /* Mark task as ready */
    if (task_set_state(task->task_id, TASK_STATE_READY) != 0) {
        klog_printf(KLOG_INFO, "unblock_task: invalid state transition for task %u\n",
                    task->task_id);
    }

    /* Add back to ready queue */
    return schedule_task(task);
}

/*
 * Terminate the currently running task and hand control to the scheduler
 */
void scheduler_task_exit(void) {
    task_t *current = scheduler.current_task;

    if (!current) {
        klog_printf(KLOG_INFO, "scheduler_task_exit: No current task\n");
        schedule();
        for (;;) {
            __asm__ volatile ("hlt");
        }
    }

    uint64_t timestamp = kdiag_timestamp();
    task_record_context_switch(current, NULL, timestamp);

    if (task_terminate((uint32_t)-1) != 0) {
        klog_printf(KLOG_INFO, "scheduler_task_exit: Failed to terminate current task\n");
    }

    uint64_t guard = sched_lock();
    scheduler.current_task = NULL;
    task_set_current(NULL);
    sched_unlock(guard);

    schedule();

    klog_printf(KLOG_INFO, "scheduler_task_exit: Schedule returned unexpectedly\n");
    for (;;) {
        __asm__ volatile ("hlt");
    }
}

/* ========================================================================
 * IDLE TASK IMPLEMENTATION
 * ======================================================================== */

/*
 * Idle task function - runs when no other tasks are ready
 */
static void idle_task_function(void *arg) {
    (void)arg;  /* Unused parameter */

    while (1) {
        /* Wake interactive tasks if an input source reports pending data */
        if (idle_wakeup_cb && idle_wakeup_cb()) {
            yield();
            continue;
        }

        /* Simple idle loop - could implement power management here */
        scheduler.idle_time++;

        /* Check if we should exit (for testing purposes) */
        /* If there are no user tasks and we're in a test environment, exit */
        if (is_kernel_initialized() && scheduler.idle_time > 1000) {
            /* Count active tasks */
            uint32_t active_tasks = 0;
            get_task_stats(NULL, &active_tasks, NULL);
            if (active_tasks <= 1) {  /* Only idle task remains */
                /* Exit idle loop - return to scheduler caller */
                break;
            }
        }

        /* Yield periodically to check for new tasks */
        if (scheduler.idle_time % 1000 == 0) {
            yield();
        }
    }

    /* Return to scheduler - this should only happen in test scenarios */
    scheduler.enabled = 0;  /* Disable scheduler */
}

/* ========================================================================
 * INITIALIZATION AND CONFIGURATION
 * ======================================================================== */

/*
 * Initialize the scheduler system
 */
int init_scheduler(void) {
    /* Initialize ready queue */
    ready_queue_init(&scheduler.ready_queue);
    spinlock_init(&scheduler_lock);

    /* Initialize scheduler state */
    scheduler.current_task = NULL;
    scheduler.idle_task = NULL;
    scheduler.policy = SCHED_POLICY_COOPERATIVE;
    scheduler.enabled = 0;  /* Start disabled */
    scheduler.time_slice = SCHED_DEFAULT_TIME_SLICE;
    scheduler.total_switches = 0;
    scheduler.total_yields = 0;
    scheduler.idle_time = 0;
    scheduler.schedule_calls = 0;
    scheduler.total_ticks = 0;
    scheduler.total_preemptions = 0;
    scheduler.preemption_enabled = scheduler_preemption_default;
    scheduler.reschedule_pending = 0;
    scheduler.in_schedule = 0;

    return 0;
}

void scheduler_register_idle_wakeup_callback(scheduler_idle_wakeup_cb_t callback) {
    idle_wakeup_cb = callback;
}

/*
 * Create and start the idle task
 */
int create_idle_task(void) {
    /* Create idle task using task management functions */
    uint32_t idle_task_id = task_create("idle", idle_task_function, NULL,
                                       3, TASK_FLAG_KERNEL_MODE);  /* Low priority, kernel mode */

    if (idle_task_id == INVALID_TASK_ID) {
        return -1;
    }

    /* Get idle task pointer */
    task_t *idle_task;
    if (task_get_info(idle_task_id, &idle_task) != 0) {
        return -1;
    }

    scheduler.idle_task = idle_task;
    return 0;
}

static int boot_step_scheduler_init(void) {
    klog_debug("Initializing scheduler subsystem...");
    return init_scheduler();
}

static int boot_step_idle_task(void) {
    klog_debug("Creating idle task...");
    return create_idle_task();
}

BOOT_INIT_STEP_WITH_FLAGS(services, "scheduler", boot_step_scheduler_init, BOOT_INIT_PRIORITY(30));
BOOT_INIT_STEP_WITH_FLAGS(services, "idle task", boot_step_idle_task, BOOT_INIT_PRIORITY(50));

/*
 * Start the scheduler (enable scheduling)
 */
int start_scheduler(void) {
    if (scheduler.enabled) {
        return -1;
    }

    scheduler.enabled = 1;

    /* Save current context as return context for testing */
    init_kernel_context(&scheduler.return_context);

    /* Stay cooperative unless explicitly requested. */
    scheduler_set_preemption_enabled(scheduler_preemption_default);

    /* If we have tasks in ready queue, start scheduling */
    if (!ready_queue_empty(&scheduler.ready_queue)) {
        schedule();
    } else if (scheduler.idle_task) {
        /* Start with idle task */
        switch_to_task(scheduler.idle_task);
    } else {
        return -1;
    }

    /* If we get here, scheduler has exited and switched back to return context */
    return 0;
}

/*
 * Stop the scheduler
 */
void stop_scheduler(void) {
    scheduler.enabled = 0;
}

/*
 * Prepare scheduler for shutdown and clear scheduling state
 */
void scheduler_shutdown(void) {
    if (scheduler.enabled) {
        stop_scheduler();
    }

    ready_queue_init(&scheduler.ready_queue);
    scheduler.current_task = NULL;
    scheduler.idle_task = NULL;
}

/* ========================================================================
 * QUERY AND STATISTICS FUNCTIONS
 * ======================================================================== */

/*
 * Get scheduler statistics
 */
void get_scheduler_stats(uint64_t *context_switches, uint64_t *yields,
                        uint32_t *ready_tasks, uint32_t *schedule_calls) {
    if (context_switches) {
        *context_switches = scheduler.total_switches;
    }
    if (yields) {
        *yields = scheduler.total_yields;
    }
    if (ready_tasks) {
        *ready_tasks = scheduler.ready_queue.count;
    }
    if (schedule_calls) {
        *schedule_calls = scheduler.schedule_calls;
    }
}

/*
 * Check if scheduler is enabled
 */
int scheduler_is_enabled(void) {
    return scheduler.enabled;
}

/*
 * Get current task from scheduler
 */
task_t *scheduler_get_current_task(void) {
    return scheduler.current_task;
}

void scheduler_set_preemption_enabled(int enabled) {
    uint64_t guard = sched_lock();
    scheduler.preemption_enabled = enabled ? 1 : 0;
    if (scheduler.preemption_enabled) {
        pit_enable_irq();
    } else {
        scheduler.reschedule_pending = 0;
        pit_disable_irq();
    }
    sched_unlock(guard);
}

int scheduler_is_preemption_enabled(void) {
    return scheduler.preemption_enabled;
}

void scheduler_timer_tick(void) {
    uint64_t guard = sched_lock();
    scheduler.total_ticks++;

    if (!scheduler.enabled || !scheduler.preemption_enabled) {
        sched_unlock(guard);
        return;
    }

    task_t *current = scheduler.current_task;
    if (!current) {
        sched_unlock(guard);
        return;
    }

    if (scheduler.in_schedule) {
        sched_unlock(guard);
        return;
    }

    if (current == scheduler.idle_task) {
        if (scheduler.ready_queue.count > 0) {
            scheduler.reschedule_pending = 1;
        }
        sched_unlock(guard);
        return;
    }

    if (current->flags & TASK_FLAG_NO_PREEMPT) {
        sched_unlock(guard);
        return;
    }

    if (current->time_slice_remaining > 0) {
        current->time_slice_remaining--;
    }

    if (current->time_slice_remaining > 0) {
        sched_unlock(guard);
        return;
    }

    if (scheduler.ready_queue.count == 0) {
        scheduler_reset_task_quantum(current);
        sched_unlock(guard);
        return;
    }

    if (!scheduler.reschedule_pending) {
        scheduler.total_preemptions++;
    }
    scheduler.reschedule_pending = 1;
    sched_unlock(guard);
}

void scheduler_request_reschedule_from_interrupt(void) {
    if (!scheduler.enabled || !scheduler.preemption_enabled) {
        return;
    }

    uint64_t guard = sched_lock();
    if (!scheduler.in_schedule) {
        scheduler.reschedule_pending = 1;
    }
    sched_unlock(guard);
}

void scheduler_handle_post_irq(void) {
    uint64_t guard = sched_lock();
    if (!scheduler.reschedule_pending) {
        sched_unlock(guard);
        return;
    }

    if (!scheduler.enabled || !scheduler.preemption_enabled) {
        scheduler.reschedule_pending = 0;
        sched_unlock(guard);
        return;
    }

    if (scheduler.in_schedule) {
        sched_unlock(guard);
        return;
    }

    scheduler.reschedule_pending = 0;
    sched_unlock(guard);
    schedule();
}
