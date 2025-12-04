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

#endif /* LIB_CPU_H */

