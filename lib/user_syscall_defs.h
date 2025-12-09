/*
 * Shared syscall argument structures for user-mode helpers.
 * These definitions are included by both kernel-side syscall handlers and
 * user-mode inline stubs to keep layouts in sync.
 */

#ifndef LIB_USER_SYSCALL_DEFS_H
#define LIB_USER_SYSCALL_DEFS_H

#include <stdint.h>
#include <stddef.h>

typedef struct user_fb_info {
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint8_t bpp;
    uint8_t pixel_format;
} user_fb_info_t;

typedef struct user_rect {
    int32_t x;
    int32_t y;
    int32_t width;
    int32_t height;
    uint32_t color;
} user_rect_t;

typedef struct user_line {
    int32_t x0;
    int32_t y0;
    int32_t x1;
    int32_t y1;
    uint32_t color;
} user_line_t;

typedef struct user_circle {
    int32_t cx;
    int32_t cy;
    int32_t radius;
    uint32_t color;
} user_circle_t;

typedef struct user_text {
    int32_t x;
    int32_t y;
    uint32_t fg_color;
    uint32_t bg_color;
    const char *str;   /* User pointer to UTF-8 string */
    uint32_t len;      /* Bytes to copy (excluding terminator), will be capped */
} user_text_t;

typedef struct user_fs_entry {
    char name[64];
    uint8_t type;   /* 0=file, 1=dir */
    uint32_t size;
} user_fs_entry_t;

typedef struct user_fs_stat {
    uint8_t type;    /* 0=file,1=dir,0xFF=missing */
    uint32_t size;
} user_fs_stat_t;

typedef struct user_fs_list {
    user_fs_entry_t *entries; /* user buffer */
    uint32_t max_entries;     /* capacity in entries */
    uint32_t count;           /* filled by kernel */
} user_fs_list_t;

typedef struct user_sys_info {
    uint32_t total_pages;
    uint32_t free_pages;
    uint32_t allocated_pages;
    uint32_t total_tasks;
    uint32_t active_tasks;
    uint64_t task_context_switches;
    uint64_t scheduler_context_switches;
    uint64_t scheduler_yields;
    uint32_t ready_tasks;
    uint32_t schedule_calls;
} user_sys_info_t;

#endif /* LIB_USER_SYSCALL_DEFS_H */


