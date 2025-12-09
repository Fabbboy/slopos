#include "syscall_common.h"
#include "../fs/fileio.h"
#include "../fs/ramfs.h"
#include "../lib/user_syscall_defs.h"
#include "../mm/user_copy.h"
#include "../mm/user_copy_helpers.h"
#include "../lib/string.h"
#include "../lib/klog.h"
#include "../mm/mm_constants.h"
#include "../mm/kernel_heap.h"

#define USER_FS_MAX_ENTRIES 64

static enum syscall_disposition syscall_fs_error(struct interrupt_frame *frame) {
    return syscall_return_err(frame, -1);
}

enum syscall_disposition syscall_fs_open(task_t *task, struct interrupt_frame *frame) {
    if (!task || task->process_id == INVALID_PROCESS_ID) {
        return syscall_fs_error(frame);
    }
    char path[USER_PATH_MAX];
    if (syscall_copy_user_str(path, sizeof(path), (const char *)frame->rdi) != 0) {
        return syscall_fs_error(frame);
    }

    uint32_t flags = (uint32_t)frame->rsi;
    int fd = file_open_for_process(task->process_id, path, flags);
    if (fd < 0) {
        return syscall_fs_error(frame);
    }
    return syscall_return_ok(frame, (uint64_t)fd);
}

enum syscall_disposition syscall_fs_close(task_t *task, struct interrupt_frame *frame) {
    if (!task || task->process_id == INVALID_PROCESS_ID) {
        return syscall_fs_error(frame);
    }
    if (file_close_fd(task->process_id, (int)frame->rdi) != 0) {
        return syscall_fs_error(frame);
    }
    return syscall_return_ok(frame, 0);
}

enum syscall_disposition syscall_fs_read(task_t *task, struct interrupt_frame *frame) {
    if (!task || task->process_id == INVALID_PROCESS_ID || !frame->rsi) {
        return syscall_fs_error(frame);
    }
    char tmp[USER_IO_MAX_BYTES];
    size_t request_len = frame->rdx > USER_IO_MAX_BYTES ? USER_IO_MAX_BYTES : (size_t)frame->rdx;
    ssize_t bytes = file_read_fd(task->process_id, (int)frame->rdi, tmp, request_len);
    if (bytes < 0) {
        return syscall_fs_error(frame);
    }
    if (syscall_copy_to_user_bounded((void *)frame->rsi, tmp, (size_t)bytes) != 0) {
        return syscall_fs_error(frame);
    }
    return syscall_return_ok(frame, (uint64_t)bytes);
}

enum syscall_disposition syscall_fs_write(task_t *task, struct interrupt_frame *frame) {
    if (!task || task->process_id == INVALID_PROCESS_ID || !frame->rsi) {
        return syscall_fs_error(frame);
    }
    char tmp[USER_IO_MAX_BYTES];
    size_t write_len = 0;
    if (syscall_bounded_from_user(tmp, sizeof(tmp), (const void *)frame->rsi,
                                  frame->rdx, USER_IO_MAX_BYTES, &write_len) != 0) {
        return syscall_fs_error(frame);
    }

    ssize_t bytes = file_write_fd(task->process_id, (int)frame->rdi, tmp, write_len);
    if (bytes < 0) {
        return syscall_fs_error(frame);
    }
    return syscall_return_ok(frame, (uint64_t)bytes);
}

enum syscall_disposition syscall_fs_stat(task_t *task, struct interrupt_frame *frame) {
    if (!task || !frame->rdi || !frame->rsi) {
        return syscall_fs_error(frame);
    }
    char path[USER_PATH_MAX];
    if (syscall_copy_user_str(path, sizeof(path), (const char *)frame->rdi) != 0) {
        return syscall_fs_error(frame);
    }

    ramfs_node_t *node = ramfs_acquire_node(path);
    if (!node) {
        return syscall_fs_error(frame);
    }

    user_fs_stat_t stat = {0};
    stat.size = (uint32_t)ramfs_get_size(node);
    stat.type = (node->type == RAMFS_TYPE_DIRECTORY) ? 1 : (node->type == RAMFS_TYPE_FILE ? 0 : 0xFF);
    ramfs_node_release(node);

    if (syscall_copy_to_user_bounded((void *)frame->rsi, &stat, sizeof(stat)) != 0) {
        return syscall_fs_error(frame);
    }

    return syscall_return_ok(frame, 0);
}

enum syscall_disposition syscall_fs_mkdir(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char path[USER_PATH_MAX];
    if (syscall_copy_user_str(path, sizeof(path), (const char *)frame->rdi) != 0) {
        return syscall_fs_error(frame);
    }

    if (!ramfs_create_directory(path)) {
        return syscall_fs_error(frame);
    }
    return syscall_return_ok(frame, 0);
}

enum syscall_disposition syscall_fs_unlink(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char path[USER_PATH_MAX];
    if (syscall_copy_user_str(path, sizeof(path), (const char *)frame->rdi) != 0) {
        return syscall_fs_error(frame);
    }

    if (file_unlink_path(path) != 0) {
        return syscall_fs_error(frame);
    }
    return syscall_return_ok(frame, 0);
}

enum syscall_disposition syscall_fs_list(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char path[USER_PATH_MAX];
    if (syscall_copy_user_str(path, sizeof(path), (const char *)frame->rdi) != 0 || frame->rsi == 0) {
        return syscall_fs_error(frame);
    }

    user_fs_list_t list_hdr;
    if (user_copy_from_user(&list_hdr, (const void *)frame->rsi, sizeof(list_hdr)) != 0) {
        return syscall_fs_error(frame);
    }

    uint32_t cap = list_hdr.max_entries;
    if (cap == 0 || cap > USER_FS_MAX_ENTRIES || !list_hdr.entries) {
        return syscall_fs_error(frame);
    }

    ramfs_node_t **entries = NULL;
    int count = 0;
    if (ramfs_list_directory(path, &entries, &count) != 0) {
        return syscall_fs_error(frame);
    }

    if (count < 0) count = 0;
    if ((uint32_t)count > cap) count = (int)cap;

    user_fs_entry_t *tmp = (user_fs_entry_t *)kmalloc(sizeof(user_fs_entry_t) * (size_t)cap);
    if (!tmp) {
        if (entries) {
            ramfs_release_list(entries, count);
            kfree(entries);
        }
        return syscall_fs_error(frame);
    }

    for (int i = 0; i < count; i++) {
        ramfs_node_t *e = entries[i];
        if (!e) {
            tmp[i].name[0] = '\0';
            tmp[i].type = 0;
            tmp[i].size = 0;
            continue;
        }
        size_t nlen = strlen(e->name);
        if (nlen >= sizeof(tmp[i].name)) {
            nlen = sizeof(tmp[i].name) - 1;
        }
        for (size_t j = 0; j < nlen; j++) {
            tmp[i].name[j] = e->name[j];
        }
        tmp[i].name[nlen] = '\0';
        tmp[i].type = (e->type == RAMFS_TYPE_DIRECTORY) ? 1 : 0;
        tmp[i].size = (uint32_t)e->size;
    }

    list_hdr.count = (uint32_t)count;

    int rc = user_copy_to_user(list_hdr.entries, tmp, sizeof(user_fs_entry_t) * (size_t)count);
    if (rc == 0) {
        rc = user_copy_to_user((void *)frame->rsi, &list_hdr, sizeof(list_hdr));
    }

    if (entries) {
        ramfs_release_list(entries, count);
        kfree(entries);
    }
    kfree(tmp);

    if (rc != 0) {
        return syscall_fs_error(frame);
    }

    return syscall_return_ok(frame, 0);
}

