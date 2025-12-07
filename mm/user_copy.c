/*
 * SlopOS User Copy Helpers
 * Validate ring3 buffers before touching them and perform guarded copies.
 */

#include "user_copy.h"

#include <stdint.h>
#include <stddef.h>
#include "../lib/memory.h"
#include "../lib/string.h"
#include "../mm/mm_constants.h"
#include "../mm/memory_layout.h"
#include "../mm/paging.h"
#include "../mm/process_vm.h"
#include "../sched/scheduler.h"
#include "../sched/task.h"

static process_page_dir_t *current_process_dir(void) {
    task_t *task = scheduler_get_current_task();
    if (!task || task->process_id == INVALID_PROCESS_ID) {
        return NULL;
    }
    return process_vm_get_page_dir(task->process_id);
}

static int validate_user_buffer(uint64_t user_ptr, size_t len, process_page_dir_t *dir) {
    if (len == 0) {
        return 0;
    }

    if (!dir) {
        return -1;
    }

    uint64_t user_start = mm_get_user_space_start();
    uint64_t user_end = mm_get_user_space_end();

    uint64_t start = user_ptr;
    uint64_t end = start + len;
    if (end < start) {
        /* Overflow */
        return -1;
    }

    if (start < user_start || end > user_end) {
        return -1;
    }

    uint64_t page = start & ~(PAGE_SIZE_4KB - 1);
    while (page < end) {
        if (virt_to_phys_in_dir(dir, page) == 0) {
            return -1;
        }
        page += PAGE_SIZE_4KB;
    }

    return 0;
}

int user_copy_from_user(void *kernel_dst, const void *user_src, size_t len) {
    process_page_dir_t *dir = current_process_dir();
    if (!kernel_dst || !user_src) {
        return -1;
    }

    if (validate_user_buffer((uint64_t)user_src, len, dir) != 0) {
        return -1;
    }

    /* Shared address space after validation: a plain memcpy suffices. */
    memcpy(kernel_dst, user_src, len);
    return 0;
}

int user_copy_to_user(void *user_dst, const void *kernel_src, size_t len) {
    process_page_dir_t *dir = current_process_dir();
    if (!user_dst || !kernel_src) {
        return -1;
    }

    if (validate_user_buffer((uint64_t)user_dst, len, dir) != 0) {
        return -1;
    }

    memcpy(user_dst, kernel_src, len);
    return 0;
}

