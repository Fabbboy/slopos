#ifndef LIB_SPINLOCK_H
#define LIB_SPINLOCK_H
/*
 * Minimal spinlock helper with IRQ save/restore.
 * This kernel is single-core; disabling interrupts is sufficient to block
 * preemption while a lock is held.
 */

#include <stdint.h>
#include "cpu.h"

typedef struct spinlock {
    volatile int locked;
} spinlock_t;

static inline void spinlock_init(spinlock_t *lock) {
    if (!lock) {
        return;
    }
    lock->locked = 0;
}

static inline uint64_t spinlock_lock_irqsave(spinlock_t *lock) {
    uint64_t flags;
    __asm__ volatile("pushfq; pop %0" : "=r"(flags));
    cpu_cli();
    while (__atomic_test_and_set(&lock->locked, __ATOMIC_ACQUIRE)) {
        __asm__ volatile("pause");
    }
    return flags;
}

static inline void spinlock_unlock_irqrestore(spinlock_t *lock, uint64_t flags) {
    __atomic_clear(&lock->locked, __ATOMIC_RELEASE);
    if (flags & (1u << 9)) {
        cpu_sti();
    }
}

static inline void spinlock_lock(spinlock_t *lock) {
    while (__atomic_test_and_set(&lock->locked, __ATOMIC_ACQUIRE)) {
        __asm__ volatile("pause");
    }
}

static inline void spinlock_unlock(spinlock_t *lock) {
    __atomic_clear(&lock->locked, __ATOMIC_RELEASE);
}

#endif /* LIB_SPINLOCK_H */


