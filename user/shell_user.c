/*
 * Full userland shell mirroring the old kernel shell logic.
 */
#include "../lib/user_syscall.h"
#include "runtime.h"
#include "shell_user.h"

#if defined(__clang__)
#pragma clang section text=".user_text" rodata=".user_rodata" data=".user_data"
#else
#pragma GCC push_options
#pragma GCC section text=".user_text" rodata=".user_rodata" data=".user_data"
#endif

/* Constants */
#define SHELL_MAX_TOKENS 16
#define SHELL_MAX_TOKEN_LENGTH 64
#define SHELL_PATH_BUF 128
#define SHELL_IO_MAX 512

/* User-facing strings */
static USER_RODATA const char prompt[] = "$ ";
static USER_RODATA const char nl[] = "\n";
static USER_RODATA const char welcome[] = "SlopOS Shell v0.1 (userland)\n";
static USER_RODATA const char help_header[] = "Available commands:\n";
static USER_RODATA const char unknown_cmd[] = "Unknown command. Type 'help'.\n";
static USER_RODATA const char path_too_long[] = "path too long\n";
static USER_RODATA const char err_no_such[] = "No such file or directory\n";
static USER_RODATA const char err_too_many_args[] = "too many arguments\n";
static USER_RODATA const char err_missing_operand[] = "missing operand\n";
static USER_RODATA const char err_missing_file[] = "missing file operand\n";
static USER_RODATA const char err_missing_text[] = "missing text operand\n";
static USER_RODATA const char halted[] = "Shell requested shutdown...\n";

/* Builtin table */
typedef int (*builtin_fn)(int argc, char **argv);
typedef struct {
    const char *name;
    builtin_fn fn;
    const char *desc;
} builtin_entry_t;

/* Buffers */
static USER_DATA char line_buf[256];
static USER_DATA char token_storage[SHELL_MAX_TOKENS][SHELL_MAX_TOKEN_LENGTH];
static USER_DATA char path_buf[SHELL_PATH_BUF];
static USER_DATA user_fs_entry_t list_entries[32];

/* Small helpers */
static USER_TEXT int u_strcmp(const char *a, const char *b) {
    if (!a || !b) {
        return (a == b) ? 0 : (a ? 1 : -1);
    }
    while (*a && (*a == *b)) {
        a++; b++;
    }
    return (unsigned char)*a - (unsigned char)*b;
}

static USER_TEXT void u_puts(const char *s) {
    if (s) {
        sys_write(s, u_strlen(s));
    }
}

static USER_TEXT int normalize_path(const char *input, char *buffer, size_t buf_sz) {
    if (!buffer || buf_sz == 0) {
        return -1;
    }
    if (!input || input[0] == '\0') {
        buffer[0] = '/';
        if (buf_sz > 1) buffer[1] = '\0';
        return 0;
    }
    if (input[0] == '/') {
        size_t len = u_strnlen(input, buf_sz - 1);
        for (size_t i = 0; i < len; i++) buffer[i] = input[i];
        buffer[len] = '\0';
        if (input[len] != '\0') return -1;
        return 0;
    }
    size_t len = u_strnlen(input, buf_sz - 2);
    if (input[len] != '\0') {
        return -1;
    }
    buffer[0] = '/';
    for (size_t i = 0; i < len; i++) buffer[i + 1] = input[i];
    buffer[len + 1] = '\0';
    return 0;
}

/* Tokenizer */
static USER_TEXT int shell_parse_line(const char *line, char **tokens, int max_tokens) {
    if (!line || !tokens || max_tokens <= 0) {
        return 0;
    }
    if (max_tokens > SHELL_MAX_TOKENS) {
        max_tokens = SHELL_MAX_TOKENS;
    }
    int count = 0;
    const char *cursor = line;
    while (*cursor != '\0') {
        while (*cursor == ' ' || *cursor == '\t' || *cursor == '\n' || *cursor == '\r') {
            cursor++;
        }
        if (*cursor == '\0') break;
        size_t token_length = 0;
        while (cursor[token_length] != '\0' &&
               cursor[token_length] != ' ' &&
               cursor[token_length] != '\t' &&
               cursor[token_length] != '\n' &&
               cursor[token_length] != '\r') {
            token_length++;
        }
        if (count >= max_tokens) {
            cursor += token_length;
            continue;
        }
        size_t copy_length = token_length;
        if (copy_length > (SHELL_MAX_TOKEN_LENGTH - 1)) {
            copy_length = SHELL_MAX_TOKEN_LENGTH - 1;
        }
        for (size_t i = 0; i < copy_length; i++) {
            token_storage[count][i] = cursor[i];
        }
        token_storage[count][copy_length] = '\0';
        tokens[count] = token_storage[count];
        count++;
        cursor += token_length;
    }
    if (count < max_tokens) {
        tokens[count] = NULL;
    }
    return count;
}

/* Builtins */
static USER_TEXT int cmd_help(int argc, char **argv);
static USER_TEXT int cmd_echo(int argc, char **argv);
static USER_TEXT int cmd_clear(int argc, char **argv);
static USER_TEXT int cmd_halt(int argc, char **argv);
static USER_TEXT int cmd_info(int argc, char **argv);
static USER_TEXT int cmd_ls(int argc, char **argv);
static USER_TEXT int cmd_cat(int argc, char **argv);
static USER_TEXT int cmd_write(int argc, char **argv);
static USER_TEXT int cmd_mkdir(int argc, char **argv);
static USER_TEXT int cmd_rm(int argc, char **argv);

static USER_RODATA const builtin_entry_t builtins[] = {
    { "help",  cmd_help,  "List available commands" },
    { "echo",  cmd_echo,  "Print arguments back to the terminal" },
    { "clear", cmd_clear, "Clear the terminal display" },
    { "halt",  cmd_halt,  "Shut down the kernel" },
    { "info",  cmd_info,  "Show kernel memory and scheduler stats" },
    { "ls",    cmd_ls,    "List directory contents" },
    { "cat",   cmd_cat,   "Display file contents" },
    { "write", cmd_write, "Write text to a file" },
    { "mkdir", cmd_mkdir, "Create a directory" },
    { "rm",    cmd_rm,    "Remove a file" }
};

static USER_TEXT const builtin_entry_t *find_builtin(const char *name) {
    for (size_t i = 0; i < (sizeof(builtins) / sizeof(builtins[0])); i++) {
        if (u_strcmp(builtins[i].name, name) == 0) {
            return &builtins[i];
        }
    }
    return NULL;
}

static USER_TEXT void print_kv(const char *k, uint64_t v) {
    char buf[64];
    size_t idx = 0;
    /* print key */
    if (k) sys_write(k, u_strlen(k));
    /* print number */
    uint64_t n = v;
    char tmp[32];
    size_t t = 0;
    if (n == 0) {
        tmp[t++] = '0';
    } else {
        while (n && t < sizeof(tmp)) {
            tmp[t++] = (char)('0' + (n % 10));
            n /= 10;
        }
    }
    while (t > 0 && idx < sizeof(buf) - 1) {
        buf[idx++] = tmp[--t];
    }
    buf[idx] = '\0';
    sys_write(buf, idx);
    sys_write(nl, 1);
}

/* Builtin implementations */
static USER_TEXT int cmd_help(int argc, char **argv) {
    (void)argc; (void)argv;
    u_puts(help_header);
    for (size_t i = 0; i < (sizeof(builtins) / sizeof(builtins[0])); i++) {
        sys_write("  ", 2);
        sys_write(builtins[i].name, u_strlen(builtins[i].name));
        sys_write(" - ", 3);
        if (builtins[i].desc) sys_write(builtins[i].desc, u_strlen(builtins[i].desc));
        sys_write(nl, 1);
    }
    return 0;
}

static USER_TEXT int cmd_echo(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        if (argv[i]) {
            sys_write(argv[i], u_strlen(argv[i]));
        }
        if (i + 1 < argc) {
            sys_write(" ", 1);
        }
    }
    sys_write(nl, 1);
    return 0;
}

static USER_TEXT int cmd_clear(int argc, char **argv) {
    (void)argc; (void)argv;
    sys_write("\x1B[2J\x1B[H", 7);
    return 0;
}

static USER_TEXT int cmd_halt(int argc, char **argv) {
    (void)argc; (void)argv;
    u_puts(halted);
    sys_halt();
    return 0;
}

static USER_TEXT int cmd_info(int argc, char **argv) {
    (void)argc; (void)argv;
    user_sys_info_t info = {0};
    if (sys_sys_info(&info) != 0) {
        sys_write("info: failed\n", 13);
        return 1;
    }
    sys_write("Kernel information:\n", 21);
    sys_write("  Memory: total pages=", 23); print_kv("", info.total_pages);
    sys_write("  Free pages=", 13); print_kv("", info.free_pages);
    sys_write("  Allocated pages=", 18); print_kv("", info.allocated_pages);
    sys_write("  Tasks: total=", 14); print_kv("", info.total_tasks);
    sys_write("  Active tasks=", 16); print_kv("", info.active_tasks);
    sys_write("  Task ctx switches=", 21); print_kv("", info.task_context_switches);
    sys_write("  Scheduler: switches=", 22); print_kv("", info.scheduler_context_switches);
    sys_write("  Yields=", 9); print_kv("", info.scheduler_yields);
    sys_write("  Ready=", 8); print_kv("", info.ready_tasks);
    sys_write("  schedule() calls=", 20); print_kv("", info.schedule_calls);
    return 0;
}

static USER_TEXT int cmd_ls(int argc, char **argv) {
    if (argc > 2) {
        u_puts(err_too_many_args);
        return 1;
    }
    const char *path = "/";
    if (argc == 2) {
        if (normalize_path(argv[1], path_buf, sizeof(path_buf)) != 0) {
            u_puts(path_too_long);
            return 1;
        }
        path = path_buf;
    }

    user_fs_list_t list = {
        .entries = list_entries,
        .max_entries = (uint32_t)(sizeof(list_entries) / sizeof(list_entries[0])),
        .count = 0
    };
    if (sys_fs_list(path, &list) != 0) {
        u_puts(err_no_such);
        return 1;
    }
    for (uint32_t i = 0; i < list.count; i++) {
        const user_fs_entry_t *e = &list_entries[i];
        if (e->type == 1) {
            sys_write("[", 1);
            sys_write(e->name, u_strlen(e->name));
            sys_write("]\n", 2);
        } else {
            sys_write(e->name, u_strlen(e->name));
            sys_write(" (", 2);
            print_kv("", e->size);
        }
    }
    return 0;
}

static USER_TEXT int cmd_cat(int argc, char **argv) {
    if (argc < 2) {
        u_puts(err_missing_file);
        return 1;
    }
    if (argc > 2) {
        u_puts(err_too_many_args);
        return 1;
    }
    if (normalize_path(argv[1], path_buf, sizeof(path_buf)) != 0) {
        u_puts(path_too_long);
        return 1;
    }
    char tmp[SHELL_IO_MAX + 1];
    long fd = sys_fs_open(path_buf, USER_FS_OPEN_READ);
    if (fd < 0) {
        u_puts(err_no_such);
        return 1;
    }
    long r = sys_fs_read((int)fd, tmp, SHELL_IO_MAX);
    sys_fs_close((int)fd);
    if (r < 0) {
        u_puts(err_no_such);
        return 1;
    }
    tmp[(r < (long)sizeof(tmp)) ? r : (long)sizeof(tmp) - 1] = '\0';
    sys_write(tmp, u_strlen(tmp));
    if (r == SHELL_IO_MAX) {
        sys_write("\n[truncated]\n", 13);
    }
    return 0;
}

static USER_TEXT int cmd_write(int argc, char **argv) {
    if (argc < 2) {
        u_puts(err_missing_file);
        return 1;
    }
    if (argc < 3) {
        u_puts(err_missing_text);
        return 1;
    }
    if (argc > 3) {
        u_puts(err_too_many_args);
        return 1;
    }
    if (normalize_path(argv[1], path_buf, sizeof(path_buf)) != 0) {
        u_puts(path_too_long);
        return 1;
    }
    const char *text = argv[2];
    size_t len = u_strlen(text);
    if (len > SHELL_IO_MAX) {
        len = SHELL_IO_MAX;
    }
    long fd = sys_fs_open(path_buf, USER_FS_OPEN_WRITE | USER_FS_OPEN_CREAT);
    if (fd < 0) {
        sys_write("write failed\n", 13);
        return 1;
    }
    long w = sys_fs_write((int)fd, text, len);
    sys_fs_close((int)fd);
    if (w < 0 || (size_t)w != len) {
        sys_write("write failed\n", 13);
        return 1;
    }
    return 0;
}

static USER_TEXT int cmd_mkdir(int argc, char **argv) {
    if (argc < 2) {
        u_puts(err_missing_operand);
        return 1;
    }
    if (argc > 2) {
        u_puts(err_too_many_args);
        return 1;
    }
    if (normalize_path(argv[1], path_buf, sizeof(path_buf)) != 0) {
        u_puts(path_too_long);
        return 1;
    }
    if (sys_fs_mkdir(path_buf) != 0) {
        sys_write("mkdir failed\n", 13);
        return 1;
    }
    return 0;
}

static USER_TEXT int cmd_rm(int argc, char **argv) {
    if (argc < 2) {
        u_puts(err_missing_operand);
        return 1;
    }
    if (argc > 2) {
        u_puts(err_too_many_args);
        return 1;
    }
    if (normalize_path(argv[1], path_buf, sizeof(path_buf)) != 0) {
        u_puts(path_too_long);
        return 1;
    }
    if (sys_fs_unlink(path_buf) != 0) {
        sys_write("rm failed\n", 9);
        return 1;
    }
    return 0;
}

USER_TEXT void shell_user_main(void *arg) {
    (void)arg;
    u_puts(welcome);
    while (1) {
        sys_write(prompt, u_strlen(prompt));
        u_memset(line_buf, 0, sizeof(line_buf));
        long len = sys_read(line_buf, sizeof(line_buf) - 1);
        if (len <= 0) {
            continue;
        }
        line_buf[(size_t)((len < (long)sizeof(line_buf)) ? len : (long)sizeof(line_buf) - 1)] = '\0';

        char *tokens[SHELL_MAX_TOKENS];
        int token_count = shell_parse_line(line_buf, tokens, SHELL_MAX_TOKENS);
        if (token_count <= 0) {
            continue;
        }
        const builtin_entry_t *b = find_builtin(tokens[0]);
        if (!b) {
            sys_write(unknown_cmd, u_strlen(unknown_cmd));
            continue;
        }
        b->fn(token_count, tokens);
    }
}

#if defined(__clang__)
#pragma clang section text="" rodata="" data=""
#else
#pragma GCC pop_options
#endif

