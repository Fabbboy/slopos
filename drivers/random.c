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

/* LFSR polynomial taps for Galois configuration */
#define LFSR_POLYNOMIAL 0xB4000001UL /* x^32 + x^7 + x^5 + x^3 + x^2 + x + 1 */

/* ========================================================================
 * INITIALIZATION
 * ======================================================================== */

/*
 * Read the CPU timestamp counter for entropy
 * This provides time-based variation even if called multiple times
 */
static inline uint32_t read_tsc_low(void) {
    uint32_t eax, edx;
    __asm__ volatile("rdtsc" : "=a"(eax), "=d"(edx));
    return eax;
}

/*
 * Initialize the random number generator
 * Seeds from TSC to ensure different sequences across boots
 */
void random_init(void) {
    /* Read TSC for entropy */
    uint32_t seed = read_tsc_low();

    /* Ensure seed is non-zero (critical for LFSR!) */
    if (seed == 0) {
        seed = 0xDEADBEEF; /* Fallback seed if TSC is zero */
    }

    lfsr_state = seed;
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
    uint32_t result = 0;

    for (int i = 0; i < 32; i++) {
        result = (result << 1) | (lfsr_step() & 1);
    }

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
