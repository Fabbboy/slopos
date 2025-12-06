/*
 * SlopOS Task Structure Definitions
 * Shared task structures and constants for task management and scheduling
 */

#ifndef SCHED_TASK_H
#define SCHED_TASK_H

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

/* ========================================================================
 * TASK CONSTANTS
 * ======================================================================== */

#define MAX_TASKS                     32        /* Maximum number of tasks */
#define TASK_STACK_SIZE               0x8000    /* 32KB default stack size */
#define TASK_NAME_MAX_LEN             32        /* Maximum task name length */
#define INVALID_TASK_ID               0xFFFFFFFF /* Invalid task ID */

/* Task states */
#define TASK_STATE_INVALID            0   /* Task slot not in use */
#define TASK_STATE_READY              1   /* Task ready to run */
#define TASK_STATE_RUNNING            2   /* Task currently executing */
#define TASK_STATE_BLOCKED            3   /* Task blocked waiting for resource */
#define TASK_STATE_TERMINATED         4   /* Task has finished execution */

/* Task priority levels (lower numbers = higher priority) */
#define TASK_PRIORITY_HIGH            0   /* High priority task */
#define TASK_PRIORITY_NORMAL          1   /* Normal priority task */
#define TASK_PRIORITY_LOW             2   /* Low priority task */
#define TASK_PRIORITY_IDLE            3   /* Idle/background task */

/* Task creation flags */
#define TASK_FLAG_USER_MODE           0x01  /* Task runs in user mode */
#define TASK_FLAG_KERNEL_MODE         0x02  /* Task runs in kernel mode */
#define TASK_FLAG_NO_PREEMPT          0x04  /* Task cannot be preempted */
#define TASK_FLAG_SYSTEM              0x08  /* System/critical task */

/* ========================================================================
 * TASK STRUCTURES
 * ======================================================================== */

/* Task entry point function signature */
typedef void (*task_entry_t)(void *arg);

/* CPU register state for context switching */
typedef struct task_context {
    /* General purpose registers */
    uint64_t rax, rbx, rcx, rdx;
    uint64_t rsi, rdi, rbp, rsp;
    uint64_t r8, r9, r10, r11;
    uint64_t r12, r13, r14, r15;

    /* Instruction pointer and flags */
    uint64_t rip;
    uint64_t rflags;

    /* Segment registers */
    uint64_t cs, ds, es, fs, gs, ss;

    /* Control registers (saved during context switch) */
    uint64_t cr3;  /* Page directory base register */
} __attribute__((packed)) task_context_t;

/* Task control block */
typedef struct task {
    uint32_t task_id;                    /* Unique task identifier */
    char name[TASK_NAME_MAX_LEN];        /* Task name for debugging */

    /* Task execution state */
    uint8_t state;                       /* Current task state */
    uint8_t priority;                    /* Task priority level */
    uint16_t flags;                      /* Task creation flags */

    /* Memory management */
    uint32_t process_id;                 /* Associated process VM space */
    uint64_t stack_base;                 /* Stack base address */
    uint64_t stack_size;                 /* Stack size in bytes */
    uint64_t stack_pointer;              /* Current stack pointer */

    /* Task entry point */
    task_entry_t entry_point;            /* Task function entry point */
    void *entry_arg;                     /* Argument passed to entry point */

    /* CPU context for switching */
    task_context_t context;              /* Saved CPU state */

    /* Scheduling information */
    uint64_t time_slice;                 /* CPU time quantum */
    uint64_t time_slice_remaining;       /* Remaining ticks in current quantum */
    uint64_t total_runtime;              /* Total CPU time used */
    uint64_t creation_time;              /* Task creation timestamp */
    uint32_t yield_count;                /* Number of voluntary yields */
    uint64_t last_run_timestamp;         /* Timestamp when task was last scheduled */
    uint32_t waiting_on_task_id;         /* Task this task is waiting on, if any */

} task_t;

void task_entry_wrapper(void);
void init_kernel_context(task_context_t *context);

/*
 * Scheduler instrumentation helpers
 */
void task_record_context_switch(task_t *from, task_t *to, uint64_t timestamp);
void task_record_yield(task_t *task);
uint64_t task_get_total_yields(void);
const char *task_state_to_string(uint8_t state);

typedef void (*task_iterate_cb)(task_t *task, void *context);
void task_iterate_active(task_iterate_cb callback, void *context);

/*
 * Task state helpers for scheduler coordination
 */
uint8_t task_get_state(const task_t *task);
bool task_is_ready(const task_t *task);
bool task_is_running(const task_t *task);
bool task_is_blocked(const task_t *task);
bool task_is_terminated(const task_t *task);

#endif /* SCHED_TASK_H */
