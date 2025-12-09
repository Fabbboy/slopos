#ifndef FS_RAMFS_H
#define FS_RAMFS_H

#include <stddef.h>
#include <stdint.h>

#define RAMFS_TYPE_FILE 1
#define RAMFS_TYPE_DIRECTORY 2

typedef struct ramfs_node {
    char *name;
    int type;
    size_t size;
    void *data;
    uint32_t refcount;
    uint8_t pending_unlink;
    struct ramfs_node *parent;
    struct ramfs_node *children;
    struct ramfs_node *next_sibling;
    struct ramfs_node *prev_sibling;
} ramfs_node_t;

int ramfs_init(void);
ramfs_node_t *ramfs_get_root(void);
ramfs_node_t *ramfs_find_node(const char *path);
ramfs_node_t *ramfs_create_directory(const char *path);
ramfs_node_t *ramfs_create_file(const char *path, const void *data, size_t size);
int ramfs_read_file(const char *path, void *buffer, size_t buffer_size, size_t *bytes_read);
int ramfs_write_file(const char *path, const void *data, size_t size);
/* Caller must kfree(*entries) when count > 0. */
int ramfs_list_directory(const char *path, ramfs_node_t ***entries, int *count);
int ramfs_remove_file(const char *path);
ramfs_node_t *ramfs_acquire_node(const char *path);
void ramfs_node_retain(ramfs_node_t *node);
void ramfs_node_release(ramfs_node_t *node);
int ramfs_read_bytes(ramfs_node_t *node, size_t offset, void *buffer, size_t buffer_len, size_t *bytes_read);
int ramfs_write_bytes(ramfs_node_t *node, size_t offset, const void *data, size_t size);
void ramfs_release_list(ramfs_node_t **entries, int count);
size_t ramfs_get_size(ramfs_node_t *node);

#endif /* FS_RAMFS_H */
