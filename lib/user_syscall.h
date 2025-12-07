/*
 * SlopOS user-mode syscall stubs (int 0x80)
 * Minimal ABI: yield, exit, write, read, roulette.
 */

#ifndef LIB_USER_SYSCALL_H
#define LIB_USER_SYSCALL_H

#include <stddef.h>
#include <stdint.h>

static inline long sys_yield(void) {
    long ret;
    __asm__ volatile(
        "mov $0, %%rax\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        :
        : "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_exit(void) {
    __asm__ volatile(
        "mov $1, %%rax\n\t"
        "int $0x80\n\t"
        :
        :
        : "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "memory");
    __builtin_unreachable();
}

static inline long sys_write(const void *buf, size_t len) {
    long ret;
    __asm__ volatile(
        "mov $2, %%rax\n\t"
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
        "mov $3, %%rax\n\t"
        "mov %1, %%rdi\n\t"
        "mov %2, %%rsi\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        : "r"(buf), "r"(len)
        : "rax", "rdi", "rsi", "rcx", "rdx", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

static inline long sys_roulette(void) {
    long ret;
    __asm__ volatile(
        "mov $4, %%rax\n\t"
        "int $0x80\n\t"
        "mov %%rax, %0\n\t"
        : "=r"(ret)
        :
        : "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "memory");
    return ret;
}

#endif /* LIB_USER_SYSCALL_H */

