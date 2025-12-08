/*
 * SlopOS Randomness Driver Implementation
 * The Chaos Engine: Spinning the wheel of fate with LFSR
 *
 * This driver implements a Linear Feedback Shift Register (LFSR) for
 * pseudorandom number generation. While deterministic, it provides
 * sufficient randomness for kernel roulette and other non-cryptographic uses.
 *
 * The LFSR is a 32-bit Galois configuration with polynomial:
 * x^32 + x^7 + x^5 + x^3 + x^2 + x + 1
 * This is a well-studied polynomial providing good statistical properties.
 */

#include "random.h"

/* ========================================================================
 * LFSR STATE AND CONFIGURATION
 * ======================================================================== */

/* Global LFSR state - the current position in the chaos spiral */
static uint32_t lfsr_state = 0;
static int random_seeded = 0;
static volatile int random_lock = 0;

/* LFSR polynomial taps for Galois configuration */
#define LFSR_POLYNOMIAL 0xB4000001UL /* x^32 + x^7 + x^5 + x^3 + x^2 + x + 1 */

/* ========================================================================
 * INITIALIZATION
 * ======================================================================== */

static inline void random_lock_acquire(void) {
    while (__sync_lock_test_and_set(&random_lock, 1)) {
        /* spin */
    }
}

static inline void random_lock_release(void) {
    __sync_lock_release(&random_lock);
}

/* Read the CPU timestamp counter for entropy */
static inline uint32_t read_tsc_low(void) {
    uint32_t eax, edx;
    __asm__ volatile("rdtsc" : "=a"(eax), "=d"(edx));
    return eax;
}

static void random_seed_state(uint32_t seed) {
    if (seed == 0) {
        seed = 0xDEADBEEF; /* Fallback seed if TSC is zero */
    }
    lfsr_state = seed;
    random_seeded = 1;
}

/*
 * Initialize the random number generator once using TSC entropy.
 */
void random_init(void) {
    if (random_seeded) {
        return;
    }

    random_lock_acquire();
    if (!random_seeded) {
        random_seed_state(read_tsc_low());
    }
    random_lock_release();
}

/* ========================================================================
 * LFSR IMPLEMENTATION
 * ======================================================================== */

/*
 * Step the LFSR one position
 * Returns the new random bit
 *
 * Galois LFSR: shifts right, taps XOR the feedback
 */
static inline uint32_t lfsr_step(void) {
    uint32_t bit = lfsr_state & 1;

    /* Shift right */
    lfsr_state >>= 1;

    /* If the output bit was 1, XOR with the polynomial */
    if (bit) {
        lfsr_state ^= LFSR_POLYNOMIAL;
    }

    return lfsr_state;
}

/* ========================================================================
 * PUBLIC API
 * ======================================================================== */

/*
 * Get the next random 32-bit number
 * Calls lfsr_step() 32 times to build a full 32-bit result
 */
uint32_t random_next(void) {
    if (!random_seeded) {
        random_init();
    }

    random_lock_acquire();
    uint32_t result = 0;
    for (int i = 0; i < 32; i++) {
        result = (result << 1) | (lfsr_step() & 1);
    }
    random_lock_release();

    return result;
}

/*
 * Get a random number in range [0, max)
 * Uses rejection sampling for even distribution
 */
uint32_t random_range(uint32_t max) {
    if (max == 0) {
        return 0;
    }

    /* Simple approach: use modulo (less uniform but good enough) */
    return random_next() % max;
}

/*
 * Get a random number in range [min, max] (inclusive)
 */
uint32_t random_range_inclusive(uint32_t min, uint32_t max) {
    if (min > max) {
        return min;
    }

    uint32_t range = (max - min) + 1;
    return min + random_range(range);
}
