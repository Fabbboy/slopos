/*
 * SlopOS user-mode syscall stubs (int 0x80)
 * Minimal ABI: yield, exit, write, read, roulette.
 */

#ifndef LIB_USER_SYSCALL_H
#define LIB_USER_SYSCALL_H

#include <stddef.h>
#include <stdint.h>
#include "syscall_numbers.h"
#include "user_syscall_defs.h"

#define _STRINGIFY2(x) #x
#define _STRINGIFY(x) _STRINGIFY2(x)

static inline long syscall_invoke(uint64_t num, uint64_t arg0, uint64_t arg1, uint64_t arg2) {
    long ret;
    __asm__ volatile(
        "mov %[num], %%rax\n\t"
        "mov %[a0], %%rdi\n\t"
        "mov %[a1], %%rsi\n\t"
        "mov %[a2], %%rdx\n\t"
        "int $0x80\n\t"
        "mov %%rax, %[ret]\n\t"
        : [ret]"=r"(ret)
        : [num]"r"(num), [a0]"r"(arg0), [a1]"r"(arg1), [a2]"r"(arg2)
        : "rax", "rdi", "rsi", "rdx", "rcx", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_yield(void) {
    return syscall_invoke(SYSCALL_YIELD, 0, 0, 0);
}

static inline long sys_exit(void) {
    syscall_invoke(SYSCALL_EXIT, 0, 0, 0);
    __builtin_unreachable();
}

static inline long sys_write(const void *buf, size_t len) {
    return syscall_invoke(SYSCALL_WRITE, (uint64_t)buf, len, 0);
}

static inline long sys_read(void *buf, size_t len) {
    return syscall_invoke(SYSCALL_READ, (uint64_t)buf, len, 0);
}

static inline uint64_t sys_roulette(void) {
    return (uint64_t)syscall_invoke(SYSCALL_ROULETTE, 0, 0, 0);
}

static inline long sys_sleep_ms(uint64_t ms) {
    return syscall_invoke(SYSCALL_SLEEP_MS, ms, 0, 0);
}

static inline long sys_fb_info(user_fb_info_t *out_info) {
    return syscall_invoke(SYSCALL_FB_INFO, (uint64_t)out_info, 0, 0);
}

static inline long sys_gfx_fill_rect(const user_rect_t *rect) {
    return syscall_invoke(SYSCALL_GFX_FILL_RECT, (uint64_t)rect, 0, 0);
}

static inline long sys_gfx_draw_line(const user_line_t *line) {
    return syscall_invoke(SYSCALL_GFX_DRAW_LINE, (uint64_t)line, 0, 0);
}

static inline long sys_gfx_draw_circle(const user_circle_t *circle) {
    return syscall_invoke(SYSCALL_GFX_DRAW_CIRCLE, (uint64_t)circle, 0, 0);
}

static inline long sys_gfx_draw_circle_filled(const user_circle_t *circle) {
    return syscall_invoke(SYSCALL_GFX_DRAW_CIRCLE_FILLED, (uint64_t)circle, 0, 0);
}

static inline long sys_font_draw(const user_text_t *text) {
    return syscall_invoke(SYSCALL_FONT_DRAW, (uint64_t)text, 0, 0);
}

static inline uint32_t sys_random_next(void) {
    return (uint32_t)syscall_invoke(SYSCALL_RANDOM_NEXT, 0, 0, 0);
}

static inline long sys_roulette_result(uint64_t fate_packed) {
    return syscall_invoke(SYSCALL_ROULETTE_RESULT, fate_packed, 0, 0);
}

static inline long sys_fs_open(const char *path, uint32_t flags) {
    return syscall_invoke(SYSCALL_FS_OPEN, (uint64_t)path, flags, 0);
}

static inline long sys_fs_close(int fd) {
    return syscall_invoke(SYSCALL_FS_CLOSE, (uint64_t)fd, 0, 0);
}

static inline long sys_fs_read(int fd, void *buf, size_t len) {
    return syscall_invoke(SYSCALL_FS_READ, (uint64_t)fd, (uint64_t)buf, len);
}

static inline long sys_fs_write(int fd, const void *buf, size_t len) {
    return syscall_invoke(SYSCALL_FS_WRITE, (uint64_t)fd, (uint64_t)buf, len);
}

static inline long sys_fs_stat(const char *path, user_fs_stat_t *out_stat) {
    return syscall_invoke(SYSCALL_FS_STAT, (uint64_t)path, (uint64_t)out_stat, 0);
}

static inline long sys_fs_mkdir(const char *path) {
    return syscall_invoke(SYSCALL_FS_MKDIR, (uint64_t)path, 0, 0);
}

static inline long sys_fs_unlink(const char *path) {
    return syscall_invoke(SYSCALL_FS_UNLINK, (uint64_t)path, 0, 0);
}

static inline long sys_fs_list(const char *path, user_fs_list_t *list) {
    return syscall_invoke(SYSCALL_FS_LIST, (uint64_t)path, (uint64_t)list, 0);
}

static inline long sys_sys_info(user_sys_info_t *info) {
    return syscall_invoke(SYSCALL_SYS_INFO, (uint64_t)info, 0, 0);
}

static inline long sys_halt(void) {
    syscall_invoke(SYSCALL_HALT, 0, 0, 0);
    __builtin_unreachable();
}

#endif /* LIB_USER_SYSCALL_H */

