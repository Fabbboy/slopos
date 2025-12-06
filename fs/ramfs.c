#include "ramfs.h"

#include <stddef.h>

#include "../mm/kernel_heap.h"
#include "../lib/string.h"
#include "../lib/memory.h"
#include "../drivers/serial.h"
#include "../lib/klog.h"

typedef enum {
    RAMFS_CREATE_NONE = 0,
    RAMFS_CREATE_DIRECTORIES = 1
} ramfs_create_mode_t;

static ramfs_node_t *ramfs_root = NULL;
static int ramfs_initialized = 0;

static void ramfs_link_child(ramfs_node_t *parent, ramfs_node_t *child) {
    if (!parent || !child) {
        return;
    }

    child->next_sibling = parent->children;
    if (parent->children) {
        parent->children->prev_sibling = child;
    }
    parent->children = child;
}

static void ramfs_detach_node(ramfs_node_t *node) {
    if (!node || !node->parent) {
        return;
    }

    ramfs_node_t *parent = node->parent;

    if (parent->children == node) {
        parent->children = node->next_sibling;
    }

    if (node->prev_sibling) {
        node->prev_sibling->next_sibling = node->next_sibling;
    }

    if (node->next_sibling) {
        node->next_sibling->prev_sibling = node->prev_sibling;
    }

    node->parent = NULL;
    node->prev_sibling = NULL;
    node->next_sibling = NULL;
}

static void ramfs_free_node_recursive(ramfs_node_t *node) {
    if (!node) {
        return;
    }

    ramfs_node_t *child = node->children;
    while (child) {
        ramfs_node_t *next = child->next_sibling;
        ramfs_free_node_recursive(child);
        child = next;
    }

    if (node->data) {
        kfree(node->data);
        node->data = NULL;
    }

    if (node->name) {
        kfree(node->name);
        node->name = NULL;
    }

    kfree(node);
}

static ramfs_node_t *ramfs_allocate_node(const char *name, size_t name_len, int type, ramfs_node_t *parent) {
    ramfs_node_t *node = kmalloc(sizeof(ramfs_node_t));
    if (!node) {
        return NULL;
    }

    char *name_copy = kmalloc(name_len + 1);
    if (!name_copy) {
        kfree(node);
        return NULL;
    }

    memcpy(name_copy, name, name_len);
    name_copy[name_len] = '\0';

    node->name = name_copy;
    node->type = type;
    node->size = 0;
    node->data = NULL;
    node->parent = parent;
    node->children = NULL;
    node->next_sibling = NULL;
    node->prev_sibling = NULL;

    return node;
}

static ramfs_node_t *ramfs_find_child_component(ramfs_node_t *parent, const char *name, size_t name_len) {
    if (!parent || parent->type != RAMFS_TYPE_DIRECTORY) {
        return NULL;
    }

    ramfs_node_t *child = parent->children;
    while (child) {
        size_t existing_len = strlen(child->name);
        if (existing_len == name_len && strncmp(child->name, name, name_len) == 0) {
            return child;
        }
        child = child->next_sibling;
    }

    return NULL;
}

static ramfs_node_t *ramfs_create_directory_child(ramfs_node_t *parent, const char *name, size_t name_len) {
    ramfs_node_t *node = ramfs_allocate_node(name, name_len, RAMFS_TYPE_DIRECTORY, parent);
    if (!node) {
        return NULL;
    }

    ramfs_link_child(parent, node);
    return node;
}

static int ramfs_component_is_dot(const char *start, size_t len) {
    return (len == 1 && start[0] == '.');
}

static int ramfs_component_is_dotdot(const char *start, size_t len) {
    return (len == 2 && start[0] == '.' && start[1] == '.');
}

static const char *ramfs_skip_slashes(const char *path) {
    while (*path == '/') {
        path++;
    }
    return path;
}

static ramfs_node_t *ramfs_traverse_internal(
    const char *path,
    ramfs_create_mode_t create_mode,
    int stop_before_last,
    const char **last_component,
    size_t *last_component_len
) {
    if (!path || path[0] != '/' || !ramfs_root) {
        return NULL;
    }

    ramfs_node_t *current = ramfs_root;
    const char *cursor = path;

    cursor = ramfs_skip_slashes(cursor);
    if (*cursor == '\0') {
        if (stop_before_last && last_component) {
            *last_component = NULL;
        }
        if (stop_before_last && last_component_len) {
            *last_component_len = 0;
        }
        return current;
    }

    while (*cursor) {
        const char *component_start = cursor;

        while (*cursor && *cursor != '/') {
            cursor++;
        }

        size_t component_len = (size_t)(cursor - component_start);

        cursor = ramfs_skip_slashes(cursor);
        int is_last = (*cursor == '\0');

        if (stop_before_last && is_last) {
            if (last_component) {
                *last_component = component_start;
            }
            if (last_component_len) {
                *last_component_len = component_len;
            }
            return current;
        }

        if (ramfs_component_is_dot(component_start, component_len)) {
            continue;
        }

        if (ramfs_component_is_dotdot(component_start, component_len)) {
            if (current->parent) {
                current = current->parent;
            }
            continue;
        }

        ramfs_node_t *next = ramfs_find_child_component(current, component_start, component_len);

        if (!next) {
            if (create_mode == RAMFS_CREATE_DIRECTORIES) {
                next = ramfs_create_directory_child(current, component_start, component_len);
                if (!next) {
                    return NULL;
                }
            } else {
                return NULL;
            }
        }

        current = next;
    }

    return current;
}

static int ramfs_validate_path(const char *path) {
    return (path && path[0] == '/');
}

static ramfs_node_t *ramfs_create_directory_internal(ramfs_node_t *parent, const char *name, size_t name_len) {
    if (!parent || parent->type != RAMFS_TYPE_DIRECTORY) {
        return NULL;
    }

    ramfs_node_t *existing = ramfs_find_child_component(parent, name, name_len);
    if (existing) {
        if (existing->type == RAMFS_TYPE_DIRECTORY) {
            return existing;
        }
        return NULL;
    }

    return ramfs_create_directory_child(parent, name, name_len);
}

ramfs_node_t *ramfs_get_root(void) {
    return ramfs_root;
}

int ramfs_init(void) {
    if (ramfs_initialized) {
        return 0;
    }

    const char root_name[] = "/";
    ramfs_node_t *root = ramfs_allocate_node(root_name, 1, RAMFS_TYPE_DIRECTORY, NULL);
    if (!root) {
        return -1;
    }

    ramfs_root = root;
    ramfs_initialized = 1;

    // Optional sample structure to verify functionality quickly
    ramfs_create_directory("/etc");
    const char sample_text[] = "SlopOS ramfs online\n";
    ramfs_create_file("/etc/readme.txt", sample_text, sizeof(sample_text) - 1);
    ramfs_create_directory("/tmp");

    klog_debug("RamFS initialized");
    return 0;
}

ramfs_node_t *ramfs_find_node(const char *path) {
    if (!ramfs_validate_path(path)) {
        return NULL;
    }

    return ramfs_traverse_internal(path, RAMFS_CREATE_NONE, 0, NULL, NULL);
}

ramfs_node_t *ramfs_create_directory(const char *path) {
    if (!ramfs_validate_path(path) || !ramfs_root) {
        return NULL;
    }

    const char *last_component = NULL;
    size_t last_len = 0;
    ramfs_node_t *parent = ramfs_traverse_internal(path, RAMFS_CREATE_DIRECTORIES, 1, &last_component, &last_len);

    if (!parent || !last_component || last_len == 0) {
        return NULL;
    }

    if (ramfs_component_is_dot(last_component, last_len) ||
        ramfs_component_is_dotdot(last_component, last_len)) {
        return parent;
    }

    return ramfs_create_directory_internal(parent, last_component, last_len);
}

ramfs_node_t *ramfs_create_file(const char *path, const void *data, size_t size) {
    if (!ramfs_validate_path(path) || !ramfs_root) {
        return NULL;
    }

    const char *last_component = NULL;
    size_t last_len = 0;
    ramfs_node_t *parent = ramfs_traverse_internal(path, RAMFS_CREATE_DIRECTORIES, 1, &last_component, &last_len);

    if (!parent || !last_component || last_len == 0) {
        return NULL;
    }

    if (ramfs_component_is_dot(last_component, last_len) ||
        ramfs_component_is_dotdot(last_component, last_len)) {
        return NULL;
    }

    ramfs_node_t *existing = ramfs_find_child_component(parent, last_component, last_len);
    if (existing) {
        if (existing->type == RAMFS_TYPE_FILE) {
            return NULL;
        }
        return NULL;
    }

    ramfs_node_t *node = ramfs_allocate_node(last_component, last_len, RAMFS_TYPE_FILE, parent);
    if (!node) {
        return NULL;
    }

    if (size > 0) {
        node->data = kmalloc(size);
        if (!node->data) {
            kfree(node->name);
            kfree(node);
            return NULL;
        }

        node->size = size;
        if (data) {
            memcpy(node->data, data, size);
        } else {
            memset(node->data, 0, size);
        }
    }

    ramfs_link_child(parent, node);
    return node;
}

int ramfs_read_file(const char *path, void *buffer, size_t buffer_size, size_t *bytes_read) {
    if (bytes_read) {
        *bytes_read = 0;
    }

    if (!ramfs_validate_path(path) || (!buffer && buffer_size > 0)) {
        return -1;
    }

    ramfs_node_t *node = ramfs_find_node(path);
    if (!node || node->type != RAMFS_TYPE_FILE) {
        return -1;
    }

    size_t readable = node->size;
    if (buffer_size < readable) {
        readable = buffer_size;
    }

    if (readable > 0) {
        memcpy(buffer, node->data, readable);
    }

    if (bytes_read) {
        *bytes_read = readable;
    }

    return 0;
}

int ramfs_write_file(const char *path, const void *data, size_t size) {
    if (!ramfs_validate_path(path)) {
        return -1;
    }

    if (size > 0 && !data) {
        return -1;
    }

    ramfs_node_t *node = ramfs_find_node(path);
    if (!node) {
        ramfs_node_t *created = ramfs_create_file(path, data, size);
        return created ? 0 : -1;
    }

    if (node->type != RAMFS_TYPE_FILE) {
        return -1;
    }

    if (size == 0) {
        if (node->data) {
            kfree(node->data);
            node->data = NULL;
        }
        node->size = 0;
        return 0;
    }

    void *new_buffer = kmalloc(size);
    if (!new_buffer) {
        return -1;
    }

    memcpy(new_buffer, data, size);

    if (node->data) {
        kfree(node->data);
    }

    node->data = new_buffer;
    node->size = size;
    return 0;
}

int ramfs_list_directory(const char *path, ramfs_node_t ***entries, int *count) {
    if (count) {
        *count = 0;
    }
    if (entries) {
        *entries = NULL;
    }

    if (!ramfs_validate_path(path) || !entries || !count) {
        return -1;
    }

    ramfs_node_t *dir = ramfs_find_node(path);
    if (!dir || dir->type != RAMFS_TYPE_DIRECTORY) {
        return -1;
    }

    int child_count = 0;
    ramfs_node_t *child = dir->children;
    while (child) {
        child_count++;
        child = child->next_sibling;
    }

    if (child_count == 0) {
        *count = 0;
        *entries = NULL;
        return 0;
    }

    ramfs_node_t **array = kmalloc(sizeof(ramfs_node_t *) * (size_t)child_count);
    if (!array) {
        return -1;
    }

    child = dir->children;
    for (int i = 0; i < child_count; i++) {
        array[i] = child;
        child = child->next_sibling;
    }

    *entries = array;
    *count = child_count;
    return 0;
}

int ramfs_remove_file(const char *path) {
    if (!ramfs_validate_path(path) || !ramfs_root) {
        return -1;
    }

    ramfs_node_t *node = ramfs_find_node(path);
    if (!node || node->type != RAMFS_TYPE_FILE || !node->parent) {
        return -1;
    }

    ramfs_detach_node(node);
    node->children = NULL;
    ramfs_free_node_recursive(node);
    return 0;
}
