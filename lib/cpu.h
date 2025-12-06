#ifndef LIB_CPU_H
#define LIB_CPU_H

#include <stdint.h>

static inline uint64_t cpu_read_tsc(void) {
    uint32_t low, high;
    __asm__ volatile("rdtsc" : "=a"(low), "=d"(high));
    return ((uint64_t)high << 32) | low;
}

static inline void cpu_cli(void) {
    __asm__ volatile ("cli" : : : "memory");
}

static inline void cpu_sti(void) {
    __asm__ volatile ("sti" : : : "memory");
}

static inline uint64_t cpu_read_rbp(void) {
    uint64_t rbp;
    __asm__ volatile ("movq %%rbp, %0" : "=r"(rbp));
    return rbp;
}

static inline uint64_t cpu_read_cr3(void) {
    uint64_t value;
    __asm__ volatile ("movq %%cr3, %0" : "=r"(value));
    return value;
}

static inline uint64_t read_cr3(void) {
    return cpu_read_cr3();
}

static inline uint64_t cpu_read_msr(uint32_t msr) {
    uint32_t low, high;
    __asm__ volatile ("rdmsr" : "=a"(low), "=d"(high) : "c"(msr));
    return ((uint64_t)high << 32) | low;
}

static inline uint64_t read_msr(uint32_t msr) {
    return cpu_read_msr(msr);
}

static inline void write_msr(uint32_t msr, uint64_t value) {
    uint32_t low = (uint32_t)(value & 0xFFFFFFFF);
    uint32_t high = (uint32_t)(value >> 32);
    __asm__ volatile ("wrmsr" : : "a"(low), "d"(high), "c"(msr));
}

static inline void cpuid(uint32_t leaf, uint32_t *eax, uint32_t *ebx,
                         uint32_t *ecx, uint32_t *edx) {
    __asm__ volatile ("cpuid"
                      : "=a" (*eax), "=b" (*ebx), "=c" (*ecx), "=d" (*edx)
                      : "a" (leaf));
}

#endif /* LIB_CPU_H */

