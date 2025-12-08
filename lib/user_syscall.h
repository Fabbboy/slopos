/*
 * SlopOS user-mode syscall stubs (int 0x80)
 * Minimal ABI: yield, exit, write, read, roulette.
 */

#ifndef LIB_USER_SYSCALL_H
#define LIB_USER_SYSCALL_H

#include <stddef.h>
#include <stdint.h>
#include "../drivers/syscall.h"
#include "user_syscall_defs.h"

#define _STRINGIFY2(x) #x
#define _STRINGIFY(x) _STRINGIFY2(x)

static inline long sys_yield(void) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_YIELD) ", %%rax\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        :
        : "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_exit(void) {
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_EXIT) ", %%rax\n\t"
        "int $0x80\n\t"
        :
        :
        : "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "memory");
    __builtin_unreachable();
}

static inline long sys_write(const void *buf, size_t len) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_WRITE) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "mov %2, %%rsi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(buf), "r"(len)
        : "rax", "rdi", "rsi", "rcx", "rdx", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_read(void *buf, size_t len) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_READ) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "mov %2, %%rsi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(buf), "r"(len)
        : "rax", "rdi", "rsi", "rcx", "rdx", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline uint64_t sys_roulette(void) {
    uint64_t ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_ROULETTE) ", %%rax\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        :
        : "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_sleep_ms(uint64_t ms) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_SLEEP_MS) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(ms)
        : "rax", "rdi", "rcx", "rdx", "rsi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_fb_info(user_fb_info_t *out_info) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_FB_INFO) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(out_info)
        : "rax", "rdi", "rcx", "rdx", "rsi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_gfx_fill_rect(const user_rect_t *rect) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_GFX_FILL_RECT) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(rect)
        : "rax", "rdi", "rcx", "rdx", "rsi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_gfx_draw_line(const user_line_t *line) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_GFX_DRAW_LINE) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(line)
        : "rax", "rdi", "rcx", "rdx", "rsi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_gfx_draw_circle(const user_circle_t *circle) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_GFX_DRAW_CIRCLE) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(circle)
        : "rax", "rdi", "rcx", "rdx", "rsi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_gfx_draw_circle_filled(const user_circle_t *circle) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_GFX_DRAW_CIRCLE_FILLED) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(circle)
        : "rax", "rdi", "rcx", "rdx", "rsi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_font_draw(const user_text_t *text) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_FONT_DRAW) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(text)
        : "rax", "rdi", "rcx", "rdx", "rsi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline uint32_t sys_random_next(void) {
    uint64_t ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_RANDOM_NEXT) ", %%rax\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        :
        : "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "memory");
    return (uint32_t)ret;
}

static inline long sys_roulette_result(uint64_t fate_packed) {
    long ret;
    __asm__ volatile(
        "mov $" _STRINGIFY(SYSCALL_ROULETTE_RESULT) ", %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(fate_packed)
        : "rax", "rdi", "rcx", "rdx", "rsi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

#endif /* LIB_USER_SYSCALL_H */

