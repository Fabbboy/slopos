/*
 * SlopOS Task Management
 * Basic task structures and task lifecycle management
 * Implements tasks as function pointers with allocated stacks
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../lib/kdiag.h"
#include "../lib/klog.h"
#include "../drivers/serial.h"
#include "../mm/kernel_heap.h"
#include "../mm/paging.h"
#include "task.h"
#include "scheduler.h"
#include "../boot/kernel_panic.h"

extern void task_entry_wrapper(void);

/* Process VM allocation flags (mirror mm/process_vm.c definitions) */
#define PROCESS_VM_FLAG_READ                  0x01
#define PROCESS_VM_FLAG_WRITE                 0x02
#define PROCESS_VM_FLAG_EXEC                  0x04
#define PROCESS_VM_FLAG_USER                  0x08

static inline uint64_t read_cr3(void) {
    uint64_t value;
    __asm__ volatile ("movq %%cr3, %0" : "=r"(value));
    return value;
}

/* Forward declarations */
uint32_t create_process_vm(void);
int destroy_process_vm(uint32_t process_id);
uint64_t process_vm_alloc(uint32_t process_id, uint64_t size, uint32_t flags);
int process_vm_free(uint32_t process_id, uint64_t vaddr, uint64_t size);
process_page_dir_t *process_vm_get_page_dir(uint32_t process_id);

/* Task manager structure */
typedef struct task_manager {
    task_t tasks[MAX_TASKS];             /* Task pool */
    uint32_t num_tasks;                  /* Number of active tasks */
    uint32_t next_task_id;               /* Next task ID to assign */

    /* Lifecycle statistics */
    uint64_t total_context_switches;     /* Total context switches performed */
    uint64_t total_yields;               /* Total voluntary yields */
    uint32_t tasks_created;              /* Total tasks created */
    uint32_t tasks_terminated;           /* Total tasks terminated */
} task_manager_t;

/* Global task manager instance */
static task_manager_t task_manager = {0};

/* Forward declarations */
static void release_task_dependents(uint32_t completed_task_id);

/* ========================================================================
 * UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Find task by task ID
 * Returns pointer to task, NULL if not found
 */
static task_t *find_task_by_id(uint32_t task_id) {
    for (uint32_t i = 0; i < MAX_TASKS; i++) {
        if (task_manager.tasks[i].task_id == task_id) {
            return &task_manager.tasks[i];
        }
    }
    return NULL;
}

/*
 * Find free task slot
 * Returns pointer to free task slot, NULL if none available
 */
static task_t *find_free_task_slot(void) {
    for (uint32_t i = 0; i < MAX_TASKS; i++) {
        if (task_manager.tasks[i].state == TASK_STATE_INVALID) {
            return &task_manager.tasks[i];
        }
    }
    return NULL;
}

/*
 * Release tasks that were waiting on the specified task to complete
 */
static void release_task_dependents(uint32_t completed_task_id) {
    for (uint32_t i = 0; i < MAX_TASKS; i++) {
        task_t *dependent = &task_manager.tasks[i];

        if (!task_is_blocked(dependent)) {
            continue;
        }

        if (dependent->waiting_on_task_id != completed_task_id) {
            continue;
        }

        dependent->waiting_on_task_id = INVALID_TASK_ID;

        if (unblock_task(dependent) != 0) {
            kprint("task_terminate: Failed to unblock dependent task\n");
        }
    }
}

/*
 * Initialize a task context for first execution
 */
static void init_task_context(task_t *task) {
    /* Clear all registers */
    task->context.rax = 0;
    task->context.rbx = 0;
    task->context.rcx = 0;
    task->context.rdx = 0;
    task->context.rsi = (uint64_t)task->entry_arg;  /* Task argument */
    task->context.rdi = (uint64_t)task->entry_point;  /* Task entry pointer */
    task->context.rbp = 0;
    task->context.rsp = task->stack_pointer;
    task->context.r8 = 0;
    task->context.r9 = 0;
    task->context.r10 = 0;
    task->context.r11 = 0;
    task->context.r12 = 0;
    task->context.r13 = 0;
    task->context.r14 = 0;
    task->context.r15 = 0;

    /* Set instruction pointer to task entry point */
    task->context.rip = (uint64_t)task_entry_wrapper;

    /* Set default flags register */
    task->context.rflags = 0x202;  /* IF=1 (interrupts enabled), reserved bit 1 */

    /* Set segment registers for kernel mode */
    if (task->flags & TASK_FLAG_KERNEL_MODE) {
        task->context.cs = GDT_CODE_SELECTOR;
        task->context.ds = GDT_DATA_SELECTOR;
        task->context.es = GDT_DATA_SELECTOR;
        task->context.fs = 0;
        task->context.gs = 0;
        task->context.ss = GDT_DATA_SELECTOR;  /* Stack segment must match data segment for ring 0 */
    } else {
        /* User mode tasks: segment registers will be set by user mode setup code */
        task->context.cs = 0;
        task->context.ds = 0;
        task->context.es = 0;
        task->context.fs = 0;
        task->context.gs = 0;
        task->context.ss = 0;
    }

    /* Page directory will be set by scheduler when switching */
    task->context.cr3 = 0;
}

/* ========================================================================
 * TASK LIFECYCLE MANAGEMENT
 * ======================================================================== */

/*
 * Create a new task
 * Returns task ID on success, INVALID_TASK_ID on failure
 */
uint32_t task_create(const char *name, task_entry_t entry_point, void *arg,
                     uint8_t priority, uint16_t flags) {
    if (!name || !entry_point) {
        kprint("task_create: Invalid parameters\n");
        return INVALID_TASK_ID;
    }

    if (task_manager.num_tasks >= MAX_TASKS) {
        kprint("task_create: Maximum tasks reached\n");
        return INVALID_TASK_ID;
    }

    /* Find free task slot */
    task_t *task = find_free_task_slot();
    if (!task) {
        kprint("task_create: No free task slots\n");
        return INVALID_TASK_ID;
    }

    uint32_t process_id = INVALID_PROCESS_ID;
    uint64_t stack_base = 0;

    /* Handle kernel mode tasks differently from user mode tasks */
    if (flags & TASK_FLAG_KERNEL_MODE) {
        /* Kernel tasks use kernel page directory and kernel heap */
        void *stack = kmalloc(TASK_STACK_SIZE);
        if (!stack) {
            kprint("task_create: Failed to allocate kernel stack\n");
            return INVALID_TASK_ID;
        }

        stack_base = (uint64_t)stack;
    } else {
        /* User mode tasks get their own process VM space */
        process_id = create_process_vm();
        if (process_id == INVALID_PROCESS_ID) {
            kprint("task_create: Failed to create process VM\n");
            return INVALID_TASK_ID;
        }

        /* Allocate stack for task */
        stack_base = process_vm_alloc(process_id, TASK_STACK_SIZE,
                                      PROCESS_VM_FLAG_READ |
                                      PROCESS_VM_FLAG_WRITE |
                                      PROCESS_VM_FLAG_USER);
        if (!stack_base) {
            kprint("task_create: Failed to allocate stack\n");
            destroy_process_vm(process_id);
            return INVALID_TASK_ID;
        }
    }

    /* Assign task ID */
    uint32_t task_id = task_manager.next_task_id++;

    /* Initialize task control block */
    task->task_id = task_id;

    /* Copy task name */
    const char *src = name;
    char *dst = task->name;
    for (int i = 0; i < TASK_NAME_MAX_LEN - 1 && *src; i++) {
        *dst++ = *src++;
    }
    *dst = '\0';

    task->state = TASK_STATE_READY;
    task->priority = priority;
    task->flags = flags;
    task->process_id = process_id;
    task->stack_base = stack_base;
    task->stack_size = TASK_STACK_SIZE;
    task->stack_pointer = stack_base + TASK_STACK_SIZE - 16;  /* 16-byte align */
    task->entry_point = entry_point;
    task->entry_arg = arg;
    task->time_slice = 10;  /* Default time slice */
    task->time_slice_remaining = task->time_slice;
    task->total_runtime = 0;
    task->creation_time = kdiag_timestamp();
    task->yield_count = 0;
    task->last_run_timestamp = 0;
    task->waiting_on_task_id = INVALID_TASK_ID;

    /* Initialize CPU context */
    init_task_context(task);

    /* Record page directory for context switches */
    if (flags & TASK_FLAG_KERNEL_MODE) {
        /* Kernel tasks use kernel page directory */
        task->context.cr3 = read_cr3() & ~0xFFFULL;
    } else {
        /* User mode tasks use their process page directory */
        process_page_dir_t *page_dir = process_vm_get_page_dir(process_id);
        if (page_dir && page_dir->pml4_phys) {
            task->context.cr3 = page_dir->pml4_phys;
        }
    }

    /* Update task manager */
    task_manager.num_tasks++;
    task_manager.tasks_created++;

    KLOG_BLOCK(KLOG_DEBUG, {
        kprint("Created task '");
        kprint(name);
        kprint("' with ID ");
        kprint_decimal(task_id);
        kprint("\n");
    });

    return task_id;
}

/*
 * Terminate a task and clean up resources
 */
int task_terminate(uint32_t task_id) {
    task_t *task = NULL;
    uint32_t resolved_id = task_id;

    if ((int32_t)task_id == -1) {
        task = scheduler_get_current_task();
        if (!task) {
            kprint("task_terminate: No current task to terminate\n");
            return -1;
        }
        resolved_id = task->task_id;
    } else {
        task = find_task_by_id(task_id);
    }

    if (!task || task->state == TASK_STATE_INVALID) {
        kprint("task_terminate: Task not found\n");
        return -1;
    }

    kprint("Terminating task '");
    kprint(task->name);
    kprint("' (ID ");
    kprint_decimal(resolved_id);
    kprint(")\n");

    /* Ensure task is removed from scheduler structures */
    unschedule_task(task);

    /* Finalize runtime statistics if task was running */
    if (task->last_run_timestamp != 0) {
        uint64_t now = kdiag_timestamp();
        if (now >= task->last_run_timestamp) {
            task->total_runtime += now - task->last_run_timestamp;
        }
        task->last_run_timestamp = 0;
    }

    /* Mark task as terminated */
    task->state = TASK_STATE_TERMINATED;

    /* Wake any dependents waiting on this task */
    release_task_dependents(resolved_id);

    /* Free resources based on task mode */
    if (task->process_id != INVALID_PROCESS_ID) {
        /* User mode tasks: free process VM space */
        destroy_process_vm(task->process_id);
    } else if (task->stack_base) {
        /* Kernel tasks: free stack from kernel heap */
        kfree((void *)task->stack_base);
    }

    /* Clear task control block */
    task->task_id = INVALID_TASK_ID;
    task->state = TASK_STATE_INVALID;
    task->process_id = INVALID_PROCESS_ID;
    task->stack_base = 0;
    task->stack_pointer = 0;
    task->stack_size = 0;
    task->time_slice = 0;
    task->time_slice_remaining = 0;
    task->total_runtime = 0;
    task->yield_count = 0;
    task->entry_point = NULL;
    task->entry_arg = NULL;
    task->creation_time = 0;
    task->waiting_on_task_id = INVALID_TASK_ID;
    task->last_run_timestamp = 0;

    /* Update task manager */
    if (task_manager.num_tasks > 0) {
        task_manager.num_tasks--;
    }
    task_manager.tasks_terminated++;

    return 0;
}

/*
 * Terminate all tasks except the current one
 * Used during shutdown sequences to release task resources
 */
int task_shutdown_all(void) {
    int result = 0;
    task_t *current = scheduler_get_current_task();

    for (uint32_t i = 0; i < MAX_TASKS; i++) {
        task_t *task = &task_manager.tasks[i];

        if (task->state == TASK_STATE_INVALID) {
            continue;
        }

        if (task == current) {
            continue;
        }

        if (task->task_id == INVALID_TASK_ID) {
            continue;
        }

        if (task_terminate(task->task_id) != 0) {
            result = -1;
        }
    }

    task_manager.num_tasks = 0;

    return result;
}

/*
 * Get task information
 */
int task_get_info(uint32_t task_id, task_t **task_info) {
    if (!task_info) {
        return -1;
    }

    task_t *task = find_task_by_id(task_id);
    if (!task || task->state == TASK_STATE_INVALID) {
        *task_info = NULL;
        return -1;
    }

    *task_info = task;
    return 0;
}

/*
 * Change task state
 */
static int task_state_transition_allowed(uint8_t old_state, uint8_t new_state) {
    if (old_state == new_state) {
        return 1;
    }

    switch (old_state) {
    case TASK_STATE_INVALID:
        return new_state == TASK_STATE_READY || new_state == TASK_STATE_INVALID;
    case TASK_STATE_READY:
        return new_state == TASK_STATE_RUNNING ||
               new_state == TASK_STATE_BLOCKED ||
               new_state == TASK_STATE_TERMINATED ||
               new_state == TASK_STATE_READY;
    case TASK_STATE_RUNNING:
        return new_state == TASK_STATE_READY ||
               new_state == TASK_STATE_BLOCKED ||
               new_state == TASK_STATE_TERMINATED;
    case TASK_STATE_BLOCKED:
        return new_state == TASK_STATE_READY ||
               new_state == TASK_STATE_TERMINATED ||
               new_state == TASK_STATE_BLOCKED;
    case TASK_STATE_TERMINATED:
        return new_state == TASK_STATE_INVALID ||
               new_state == TASK_STATE_TERMINATED;
    default:
        return 0;
    }
}

int task_set_state(uint32_t task_id, uint8_t new_state) {
    task_t *task = find_task_by_id(task_id);
    if (!task || task->state == TASK_STATE_INVALID) {
        return -1;
    }

    uint8_t old_state = task->state;

    if (!task_state_transition_allowed(old_state, new_state)) {
        kprint("task_set_state: invalid transition for task ");
        kprint_decimal(task_id);
        kprint(" (");
        kprint(task_state_to_string(old_state));
        kprint(" -> ");
        kprint(task_state_to_string(new_state));
        kprint(")\n");
    }

    task->state = new_state;

    if (klog_is_enabled(KLOG_DEBUG)) {
        kprint("Task ");
        kprint_decimal(task_id);
        kprint(" state: ");
        kprint_decimal(old_state);
        kprint(" -> ");
        kprint_decimal(new_state);
        kprint("\n");
    }

    return 0;
}

/* ========================================================================
 * INITIALIZATION AND QUERY FUNCTIONS
 * ======================================================================== */

/*
 * Initialize the task management system
 */
int init_task_manager(void) {
    task_manager.num_tasks = 0;
    task_manager.next_task_id = 1;
    task_manager.total_context_switches = 0;
    task_manager.total_yields = 0;
    task_manager.tasks_created = 0;
    task_manager.tasks_terminated = 0;

    /* Clear task pool */
    for (uint32_t i = 0; i < MAX_TASKS; i++) {
        task_manager.tasks[i].task_id = INVALID_TASK_ID;
        task_manager.tasks[i].state = TASK_STATE_INVALID;
        task_manager.tasks[i].process_id = INVALID_PROCESS_ID;
        task_manager.tasks[i].total_runtime = 0;
        task_manager.tasks[i].yield_count = 0;
        task_manager.tasks[i].last_run_timestamp = 0;
        task_manager.tasks[i].waiting_on_task_id = INVALID_TASK_ID;
        task_manager.tasks[i].time_slice_remaining = 0;
    }

    return 0;
}

/*
 * Get task manager statistics
 */
void get_task_stats(uint32_t *total_tasks, uint32_t *active_tasks,
                   uint64_t *context_switches) {
    if (total_tasks) {
        *total_tasks = task_manager.tasks_created;
    }
    if (active_tasks) {
        *active_tasks = task_manager.num_tasks;
    }
    if (context_switches) {
        *context_switches = task_manager.total_context_switches;
    }
}

/*
 * Record scheduler context switch information
 */
void task_record_context_switch(task_t *from, task_t *to, uint64_t timestamp) {
    if (from && from->last_run_timestamp != 0 && timestamp >= from->last_run_timestamp) {
        from->total_runtime += timestamp - from->last_run_timestamp;
    }

    if (from) {
        from->last_run_timestamp = 0;
    }

    if (to) {
        to->last_run_timestamp = timestamp;
    }

    if (to && to != from) {
        task_manager.total_context_switches++;
    }
}

/*
 * Record voluntary yield for task statistics
 */
void task_record_yield(task_t *task) {
    task_manager.total_yields++;

    if (task) {
        task->yield_count++;
    }
}

/*
 * Get number of yields recorded across all tasks
 */
uint64_t task_get_total_yields(void) {
    return task_manager.total_yields;
}

/*
 * Convert task state to string for diagnostics
 */
const char *task_state_to_string(uint8_t state) {
    switch (state) {
    case TASK_STATE_INVALID:
        return "invalid";
    case TASK_STATE_READY:
        return "ready";
    case TASK_STATE_RUNNING:
        return "running";
    case TASK_STATE_BLOCKED:
        return "blocked";
    case TASK_STATE_TERMINATED:
        return "terminated";
    default:
        return "unknown";
    }
}

/*
 * Iterate over active tasks and invoke callback
 */
void task_iterate_active(task_iterate_cb callback, void *context) {
    if (!callback) {
        return;
    }

    for (uint32_t i = 0; i < MAX_TASKS; i++) {
        task_t *task = &task_manager.tasks[i];

        if (task->state == TASK_STATE_INVALID || task->task_id == INVALID_TASK_ID) {
            continue;
        }

        callback(task, context);
    }
}

/*
 * Get current task ID
 */
uint32_t task_get_current_id(void) {
    task_t *current = scheduler_get_current_task();
    if (current) {
        return current->task_id;
    }
    return 0;  /* Kernel/no task */
}

/*
 * Get current task structure
 */
task_t *task_get_current(void) {
    return scheduler_get_current_task();
}

/*
 * Set current task (used by scheduler)
 */
void task_set_current(task_t *task) {
    if (!task) {
        return;
    }

    if (task->state != TASK_STATE_READY && task->state != TASK_STATE_RUNNING) {
        kprint("task_set_current: unexpected state transition for task ");
        kprint_decimal(task->task_id);
        kprint(" (state ");
        kprint_decimal(task->state);
        kprint(")\n");
    }

    task->state = TASK_STATE_RUNNING;
}

uint8_t task_get_state(const task_t *task) {
    if (!task) {
        return TASK_STATE_INVALID;
    }
    return task->state;
}

bool task_is_ready(const task_t *task) {
    return task_get_state(task) == TASK_STATE_READY;
}

bool task_is_running(const task_t *task) {
    return task_get_state(task) == TASK_STATE_RUNNING;
}

bool task_is_blocked(const task_t *task) {
    return task_get_state(task) == TASK_STATE_BLOCKED;
}

bool task_is_terminated(const task_t *task) {
    return task_get_state(task) == TASK_STATE_TERMINATED;
}
