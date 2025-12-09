#include "fileio.h"

#include "../lib/memory.h"
#include "../lib/string.h"
#include "../mm/kernel_heap.h"
#include "../mm/mm_constants.h"
#include "../lib/spinlock.h"

typedef struct file_table_slot {
    uint32_t process_id;
    int in_use;
    spinlock_t lock;
    file_descriptor_t descriptors[FILEIO_MAX_OPEN_FILES];
} file_table_slot_t;

static file_table_slot_t kernel_table;
static file_table_slot_t process_tables[MAX_PROCESSES];
static int fileio_initialized = 0;

static void fileio_reset_descriptor(file_descriptor_t *desc) {
    if (!desc) {
        return;
    }
    if (desc->node) {
        ramfs_node_release(desc->node);
    }
    desc->node = NULL;
    desc->position = 0;
    desc->flags = 0;
    desc->valid = 0;
}

static void fileio_reset_table(file_table_slot_t *table) {
    if (!table) {
        return;
    }
    for (int i = 0; i < FILEIO_MAX_OPEN_FILES; i++) {
        fileio_reset_descriptor(&table->descriptors[i]);
    }
}

static void fileio_init_kernel(void) {
    if (fileio_initialized) {
        return;
    }
    spinlock_init(&kernel_table.lock);
    kernel_table.in_use = 1;
    kernel_table.process_id = INVALID_PROCESS_ID;
    fileio_reset_table(&kernel_table);
    fileio_initialized = 1;
}

static file_table_slot_t *fileio_find_free_table(void) {
    for (int i = 0; i < MAX_PROCESSES; i++) {
        if (!process_tables[i].in_use) {
            return &process_tables[i];
        }
    }
    return NULL;
}

static file_table_slot_t *fileio_table_for_pid(uint32_t pid) {
    fileio_init_kernel();
    if (pid == INVALID_PROCESS_ID) {
        return &kernel_table;
    }
    for (int i = 0; i < MAX_PROCESSES; i++) {
        if (process_tables[i].in_use && process_tables[i].process_id == pid) {
            return &process_tables[i];
        }
    }
    return NULL;
}

static file_descriptor_t *fileio_get_descriptor(file_table_slot_t *table, int fd) {
    if (!table || fd < 0 || fd >= FILEIO_MAX_OPEN_FILES) {
        return NULL;
    }
    file_descriptor_t *desc = &table->descriptors[fd];
    if (!desc->valid) {
        return NULL;
    }
    return desc;
}

static int fileio_find_free_slot(file_table_slot_t *table) {
    if (!table) {
        return -1;
    }
    for (int i = 0; i < FILEIO_MAX_OPEN_FILES; i++) {
        if (!table->descriptors[i].valid) {
            return i;
        }
    }
    return -1;
}

int fileio_create_table_for_process(uint32_t process_id) {
    fileio_init_kernel();
    if (process_id == INVALID_PROCESS_ID) {
        return 0;
    }
    if (fileio_table_for_pid(process_id)) {
        return 0;
    }

    file_table_slot_t *slot = fileio_find_free_table();
    if (!slot) {
        return -1;
    }

    spinlock_init(&slot->lock);
    slot->process_id = process_id;
    slot->in_use = 1;
    fileio_reset_table(slot);
    return 0;
}

void fileio_destroy_table_for_process(uint32_t process_id) {
    fileio_init_kernel();
    if (process_id == INVALID_PROCESS_ID) {
        return;
    }
    file_table_slot_t *table = fileio_table_for_pid(process_id);
    if (!table || table == &kernel_table) {
        return;
    }
    uint64_t flags = spinlock_lock_irqsave(&table->lock);
    fileio_reset_table(table);
    table->process_id = INVALID_PROCESS_ID;
    table->in_use = 0;
    spinlock_unlock_irqrestore(&table->lock, flags);
}

int file_open_for_process(uint32_t process_id, const char *path, uint32_t flags) {
    file_table_slot_t *table = fileio_table_for_pid(process_id);
    if (!table) {
        return -1;
    }
    if (!table->in_use) {
        spinlock_init(&table->lock);
        table->process_id = process_id;
        table->in_use = 1;
        fileio_reset_table(table);
    }

    if (!path || !(flags & (FILE_OPEN_READ | FILE_OPEN_WRITE))) {
        return -1;
    }

    if ((flags & FILE_OPEN_APPEND) && !(flags & FILE_OPEN_WRITE)) {
        return -1;
    }

    uint64_t guard = spinlock_lock_irqsave(&table->lock);

    int slot = fileio_find_free_slot(table);
    if (slot < 0) {
        spinlock_unlock_irqrestore(&table->lock, guard);
        return -1;
    }

    ramfs_node_t *node = ramfs_acquire_node(path);
    if (!node && (flags & FILE_OPEN_CREAT)) {
        node = ramfs_create_file(path, NULL, 0);
        if (node) {
            ramfs_node_retain(node);
        }
    }

    if (!node || node->type != RAMFS_TYPE_FILE) {
        if (node) {
            ramfs_node_release(node);
        }
        spinlock_unlock_irqrestore(&table->lock, guard);
        return -1;
    }

    file_descriptor_t *desc = &table->descriptors[slot];
    desc->node = node;
    desc->flags = flags;
    desc->position = (flags & FILE_OPEN_APPEND) ? ramfs_get_size(node) : 0;
    desc->valid = 1;

    spinlock_unlock_irqrestore(&table->lock, guard);
    return slot;
}

ssize_t file_read_fd(uint32_t process_id, int fd, void *buffer, size_t count) {
    if (!buffer || count == 0) {
        return 0;
    }

    file_table_slot_t *table = fileio_table_for_pid(process_id);
    if (!table || !table->in_use) {
        return -1;
    }

    uint64_t guard = spinlock_lock_irqsave(&table->lock);
    file_descriptor_t *desc = fileio_get_descriptor(table, fd);
    if (!desc || !(desc->flags & FILE_OPEN_READ) || !desc->node || desc->node->type != RAMFS_TYPE_FILE) {
        spinlock_unlock_irqrestore(&table->lock, guard);
        return -1;
    }

    size_t read_len = 0;
    int rc = ramfs_read_bytes(desc->node, desc->position, buffer, count, &read_len);
    if (rc == 0) {
        desc->position += read_len;
    }
    spinlock_unlock_irqrestore(&table->lock, guard);
    return (rc == 0) ? (ssize_t)read_len : -1;
}

ssize_t file_write_fd(uint32_t process_id, int fd, const void *buffer, size_t count) {
    if (!buffer || count == 0) {
        return 0;
    }

    file_table_slot_t *table = fileio_table_for_pid(process_id);
    if (!table || !table->in_use) {
        return -1;
    }

    uint64_t guard = spinlock_lock_irqsave(&table->lock);
    file_descriptor_t *desc = fileio_get_descriptor(table, fd);
    if (!desc || !(desc->flags & FILE_OPEN_WRITE) || !desc->node || desc->node->type != RAMFS_TYPE_FILE) {
        spinlock_unlock_irqrestore(&table->lock, guard);
        return -1;
    }

    int rc = ramfs_write_bytes(desc->node, desc->position, buffer, count);
    if (rc == 0) {
        desc->position += count;
    }
    spinlock_unlock_irqrestore(&table->lock, guard);
    return (rc == 0) ? (ssize_t)count : -1;
}

int file_close_fd(uint32_t process_id, int fd) {
    file_table_slot_t *table = fileio_table_for_pid(process_id);
    if (!table || !table->in_use) {
        return -1;
    }
    uint64_t guard = spinlock_lock_irqsave(&table->lock);
    file_descriptor_t *desc = fileio_get_descriptor(table, fd);
    if (!desc) {
        spinlock_unlock_irqrestore(&table->lock, guard);
        return -1;
    }
    fileio_reset_descriptor(desc);
    spinlock_unlock_irqrestore(&table->lock, guard);
    return 0;
}

int file_seek_fd(uint32_t process_id, int fd, uint64_t offset, int whence) {
    file_table_slot_t *table = fileio_table_for_pid(process_id);
    if (!table || !table->in_use) {
        return -1;
    }
    uint64_t guard = spinlock_lock_irqsave(&table->lock);
    file_descriptor_t *desc = fileio_get_descriptor(table, fd);
    if (!desc || !desc->node || desc->node->type != RAMFS_TYPE_FILE) {
        spinlock_unlock_irqrestore(&table->lock, guard);
        return -1;
    }

    size_t new_position = desc->position;
    if (offset > SIZE_MAX) {
        spinlock_unlock_irqrestore(&table->lock, guard);
        return -1;
    }
    size_t delta = (size_t)offset;
    size_t size = ramfs_get_size(desc->node);

    switch (whence) {
        case SEEK_SET:
            if (delta > size) {
                spinlock_unlock_irqrestore(&table->lock, guard);
                return -1;
            }
            new_position = delta;
            break;
        case SEEK_CUR:
            if (delta > SIZE_MAX - desc->position || desc->position + delta > size) {
                spinlock_unlock_irqrestore(&table->lock, guard);
                return -1;
            }
            new_position = desc->position + delta;
            break;
        case SEEK_END:
            if (delta > size) {
                spinlock_unlock_irqrestore(&table->lock, guard);
                return -1;
            }
            new_position = size - delta;
            break;
        default:
            spinlock_unlock_irqrestore(&table->lock, guard);
            return -1;
    }

    desc->position = new_position;
    spinlock_unlock_irqrestore(&table->lock, guard);
    return 0;
}

size_t file_get_size_fd(uint32_t process_id, int fd) {
    file_table_slot_t *table = fileio_table_for_pid(process_id);
    if (!table || !table->in_use) {
        return (size_t)-1;
    }
    uint64_t guard = spinlock_lock_irqsave(&table->lock);
    file_descriptor_t *desc = fileio_get_descriptor(table, fd);
    size_t size = (size_t)-1;
    if (desc && desc->node && desc->node->type == RAMFS_TYPE_FILE) {
        size = ramfs_get_size(desc->node);
    }
    spinlock_unlock_irqrestore(&table->lock, guard);
    return size;
}

int file_exists_path(const char *path) {
    if (!path) {
        return 0;
    }
    ramfs_node_t *node = ramfs_find_node(path);
    if (!node || node->type != RAMFS_TYPE_FILE) {
        return 0;
    }
    return 1;
}

int file_unlink_path(const char *path) {
    if (!path) {
        return -1;
    }
    return ramfs_remove_file(path);
}
