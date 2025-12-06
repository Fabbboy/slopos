#include <stddef.h>

#include "../drivers/serial.h"
#include "../lib/string.h"
#include "../lib/memory.h"
#include "../mm/kernel_heap.h"
#include "ramfs.h"
#include "../lib/klog.h"

static int test_ramfs_root_node(void) {
    klog_printf(KLOG_INFO, "RAMFS_TEST: Verifying root node properties\n");

    if (ramfs_init() != 0) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: ramfs_init failed\n");
        return -1;
    }

    ramfs_node_t *root = ramfs_get_root();
    if (!root) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Root node is NULL\n");
        return -1;
    }

    if (root->type != RAMFS_TYPE_DIRECTORY) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Root node type is not directory\n");
        return -1;
    }

    if (root->parent != NULL) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Root node parent is not NULL\n");
        return -1;
    }

    if (strcmp(root->name, "/") != 0) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Root node name is not '/'\n");
        return -1;
    }

    klog_printf(KLOG_INFO, "RAMFS_TEST: Root node verification PASSED\n");
    return 0;
}

static ramfs_node_t *ensure_directory(const char *path) {
    ramfs_node_t *dir = ramfs_create_directory(path);
    if (!dir) {
        dir = ramfs_find_node(path);
    }
    return dir;
}

static int test_ramfs_file_roundtrip(void) {
    klog_printf(KLOG_INFO, "RAMFS_TEST: Testing file creation and readback\n");

    const char *dir_path = "/itests";
    ramfs_node_t *dir = ensure_directory(dir_path);
    if (!dir || dir->type != RAMFS_TYPE_DIRECTORY) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Failed to ensure /itests directory\n");
        return -1;
    }

    const char *file_path = "/itests/hello.txt";
    const char sample[] = "hello";
    ramfs_node_t *file = ramfs_create_file(file_path, sample, sizeof(sample) - 1);
    if (!file) {
        file = ramfs_find_node(file_path);
        if (!file || file->type != RAMFS_TYPE_FILE) {
            klog_printf(KLOG_INFO, "RAMFS_TEST: Failed to create or find /itests/hello.txt\n");
            return -1;
        }
        if (ramfs_write_file(file_path, sample, sizeof(sample) - 1) != 0) {
            klog_printf(KLOG_INFO, "RAMFS_TEST: Failed to overwrite existing /itests/hello.txt\n");
            return -1;
        }
    }

    char buffer[16] = {0};
    size_t bytes_read = 0;
    if (ramfs_read_file(file_path, buffer, sizeof(buffer), &bytes_read) != 0) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Failed to read /itests/hello.txt\n");
        return -1;
    }

    if (bytes_read != sizeof(sample) - 1 || memcmp(buffer, sample, sizeof(sample) - 1) != 0) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: File content mismatch for /itests/hello.txt\n");
        return -1;
    }

    klog_printf(KLOG_INFO, "RAMFS_TEST: File creation and readback PASSED\n");
    return 0;
}

static int test_ramfs_write_updates_file(void) {
    klog_printf(KLOG_INFO, "RAMFS_TEST: Testing file overwrite via ramfs_write_file\n");

    const char *file_path = "/itests/hello.txt";
    const char updated[] = "goodbye world";

    if (ramfs_write_file(file_path, updated, sizeof(updated) - 1) != 0) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: ramfs_write_file failed for /itests/hello.txt\n");
        return -1;
    }

    ramfs_node_t *file = ramfs_find_node(file_path);
    if (!file || file->type != RAMFS_TYPE_FILE) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: /itests/hello.txt not found after write\n");
        return -1;
    }

    if (file->size != sizeof(updated) - 1) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: File size mismatch after overwrite\n");
        return -1;
    }

    char buffer[32] = {0};
    size_t bytes_read = 0;
    if (ramfs_read_file(file_path, buffer, sizeof(buffer), &bytes_read) != 0) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Failed to read /itests/hello.txt after overwrite\n");
        return -1;
    }

    if (bytes_read != sizeof(updated) - 1 || memcmp(buffer, updated, sizeof(updated) - 1) != 0) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: File content mismatch after overwrite\n");
        return -1;
    }

    klog_printf(KLOG_INFO, "RAMFS_TEST: File overwrite test PASSED\n");
    return 0;
}

static int test_ramfs_nested_directories(void) {
    klog_printf(KLOG_INFO, "RAMFS_TEST: Testing nested directory creation and traversal\n");

    const char *nested_dir_path = "/itests/nested";
    ramfs_node_t *nested_dir = ensure_directory(nested_dir_path);
    if (!nested_dir || nested_dir->type != RAMFS_TYPE_DIRECTORY) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Failed to ensure /itests/nested directory\n");
        return -1;
    }

    const char *nested_file_path = "/itests/nested/file.txt";
    const char nested_content[] = "nested data";
    ramfs_node_t *nested_file = ramfs_create_file(nested_file_path, nested_content, sizeof(nested_content) - 1);
    if (!nested_file) {
        nested_file = ramfs_find_node(nested_file_path);
        if (!nested_file || nested_file->type != RAMFS_TYPE_FILE) {
            klog_printf(KLOG_INFO, "RAMFS_TEST: Failed to create /itests/nested/file.txt\n");
            return -1;
        }
        if (ramfs_write_file(nested_file_path, nested_content, sizeof(nested_content) - 1) != 0) {
            klog_printf(KLOG_INFO, "RAMFS_TEST: Failed to overwrite /itests/nested/file.txt\n");
            return -1;
        }
    }

    ramfs_node_t *via_dot = ramfs_find_node("/itests/nested/./file.txt");
    if (via_dot != nested_file) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Dot path resolution failed for nested file\n");
        return -1;
    }

    ramfs_node_t *via_dotdot = ramfs_find_node("/itests/nested/../nested");
    if (via_dotdot != nested_dir) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Dot-dot path resolution failed for nested directory\n");
        return -1;
    }

    klog_printf(KLOG_INFO, "RAMFS_TEST: Nested directory traversal PASSED\n");
    return 0;
}

static int test_ramfs_list_directory(void) {
    klog_printf(KLOG_INFO, "RAMFS_TEST: Testing directory listing\n");

    ramfs_node_t **entries = NULL;
    int count = 0;
    if (ramfs_list_directory("/itests", &entries, &count) != 0) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: ramfs_list_directory failed for /itests\n");
        return -1;
    }

    int found_file = 0;
    int found_nested = 0;

    for (int i = 0; i < count; i++) {
        if (!entries[i] || !entries[i]->name) {
            continue;
        }
        if (strcmp(entries[i]->name, "hello.txt") == 0) {
            found_file = 1;
        }
        if (strcmp(entries[i]->name, "nested") == 0) {
            found_nested = 1;
        }
    }

    if (entries) {
        kfree(entries);
    }

    if (!found_file || !found_nested) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Directory listing missing expected entries\n");
        return -1;
    }

    if (ramfs_list_directory("/itests/hello.txt", &entries, &count) == 0) {
        klog_printf(KLOG_INFO, "RAMFS_TEST: Listing a file should have failed\n");
        if (entries) {
            kfree(entries);
        }
        return -1;
    }

    klog_printf(KLOG_INFO, "RAMFS_TEST: Directory listing test PASSED\n");
    return 0;
}

int run_ramfs_tests(void) {
    klog_printf(KLOG_INFO, "RAMFS_TEST: Running ramfs regression tests\n");

    int passed = 0;
    int total = 0;

    total++;
    if (test_ramfs_root_node() == 0) {
        passed++;
    }

    total++;
    if (test_ramfs_file_roundtrip() == 0) {
        passed++;
    }

    total++;
    if (test_ramfs_write_updates_file() == 0) {
        passed++;
    }

    total++;
    if (test_ramfs_nested_directories() == 0) {
        passed++;
    }

    total++;
    if (test_ramfs_list_directory() == 0) {
        passed++;
    }

    klog_printf(KLOG_INFO, "RAMFS_TEST: Completed %d tests, %d passed\n", total, passed);

    return passed;
}
