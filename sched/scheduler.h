/*
 * SlopOS Scheduler Interface Header
 * Public interface for the cooperative scheduler system
 */

#ifndef SCHED_SCHEDULER_H
#define SCHED_SCHEDULER_H

#include <stdint.h>
#include <stddef.h>
#include "task.h"

/* Low-level context switch helpers implemented in assembly */
void context_switch(void *old_context, void *new_context);
void simple_context_switch(void *old_context, void *new_context);

/* ========================================================================
 * TASK MANAGEMENT FUNCTIONS
 * ======================================================================== */

/*
 * Initialize the task management system
 * Must be called before creating any tasks
 * Returns 0 on success, non-zero on failure
 */
int init_task_manager(void);

/*
 * Create a new task
 * Returns task ID on success, INVALID_TASK_ID on failure
 */
uint32_t task_create(const char *name, task_entry_t entry_point, void *arg,
                     uint8_t priority, uint16_t flags);

/*
 * Terminate a task and clean up resources
 * Returns 0 on success, non-zero on failure
 */
int task_terminate(uint32_t task_id);

/*
 * Get task information structure
 * Returns 0 on success, non-zero on failure
 */
int task_get_info(uint32_t task_id, task_t **task_info);

/*
 * Change task state
 * Returns 0 on success, non-zero on failure
 */
int task_set_state(uint32_t task_id, uint8_t new_state);

/*
 * Get current task ID
 * Returns current task ID, 0 for kernel
 */
uint32_t task_get_current_id(void);

/*
 * Get current task structure
 * Returns pointer to current task, NULL if no task running
 */
task_t *task_get_current(void);

/*
 * Set current task (used by scheduler)
 */
void task_set_current(task_t *task);

/*
 * Terminate all tasks except the current one (used for shutdown)
 * Returns 0 on success, non-zero on failure
 */
int task_shutdown_all(void);

/* ========================================================================
 * SCHEDULER FUNCTIONS
 * ======================================================================== */

/*
 * Initialize the scheduler system
 * Returns 0 on success, non-zero on failure
 */
int init_scheduler(void);

/*
 * Create and start the idle task
 * Returns 0 on success, non-zero on failure
 */
int create_idle_task(void);

/*
 * Start the scheduler (enable scheduling)
 * Returns 0 on success, non-zero on failure
 */
int start_scheduler(void);

/*
 * Stop the scheduler
 */
void stop_scheduler(void);

/*
 * Prepare scheduler for shutdown (stop scheduling and clear state)
 */
void scheduler_shutdown(void);

/*
 * Add task to ready queue for scheduling
 * Returns 0 on success, non-zero on failure
 */
int schedule_task(task_t *task);

/*
 * Remove task from scheduler
 * Returns 0 on success, non-zero on failure
 */
int unschedule_task(task_t *task);

/*
 * Main scheduling function - select and switch to next task
 */
void schedule(void);

/*
 * Yield CPU voluntarily (cooperative scheduling)
 */
void yield(void);

/*
 * Block current task (remove from ready queue)
 */
void block_current_task(void);

/*
 * Block the current task until the specified task terminates
 */
int task_wait_for(uint32_t task_id);

/*
 * Unblock task (add back to ready queue)
 * Returns 0 on success, non-zero on failure
 */
int unblock_task(task_t *task);

/*
 * Terminate the current task and reschedule
 */
void scheduler_task_exit(void) __attribute__((noreturn));

/*
 * Check if scheduler is enabled
 * Returns non-zero if enabled, zero if disabled
 */
int scheduler_is_enabled(void);

/*
 * Enable or disable preemption globally
 */
void scheduler_set_preemption_enabled(int enabled);

/*
 * Check if preemption is enabled
 */
int scheduler_is_preemption_enabled(void);

/*
 * Get current task from scheduler
 */
task_t *scheduler_get_current_task(void);

/*
 * Timer tick handler for the scheduler
 */
void scheduler_timer_tick(void);

/*
 * Handle deferred rescheduling after interrupt processing
 */
void scheduler_handle_post_irq(void);

/*
 * Request a reschedule from interrupt context.
 * Marks that the scheduler should pick a new task after the ISR returns.
 */
void scheduler_request_reschedule_from_interrupt(void);

/* ========================================================================
 * STATISTICS AND MONITORING
 * ======================================================================== */

/*
 * Get scheduler statistics
 */
void get_scheduler_stats(uint64_t *context_switches, uint64_t *yields,
                        uint32_t *ready_tasks, uint32_t *schedule_calls);

/*
 * Get task manager statistics
 */
void get_task_stats(uint32_t *total_tasks, uint32_t *active_tasks,
                   uint64_t *context_switches);

/* ========================================================================
 * TEST FUNCTIONS
 * ======================================================================== */

/*
 * Run basic scheduler test with two cooperative tasks
 * Returns 0 on success, non-zero on failure
 */
int run_scheduler_test(void);

/*
 * Print current scheduler statistics
 */
void print_scheduler_stats(void);

#endif /* SCHED_SCHEDULER_H */
