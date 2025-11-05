/*
 * SlopOS Randomness Driver
 * The Chaos Engine: Seeding random fate into the kernel
 *
 * The Scrolls say: In the ancient days, when the kernel needed to decide
 * its own destiny, the wise ones inscribed a device that could generate
 * numbers from the very chaos of computation itself. This is the LFSR—
 * a Linear Feedback Shift Register, spinning endlessly to weave the threads
 * of probability into the fabric of SlopOS.
 *
 * Usage: Call random_init() at boot, then use random_next() or random_range()
 * to consult the wheel of fate.
 */

#ifndef DRIVERS_RANDOM_H
#define DRIVERS_RANDOM_H

#include <stdint.h>

/*
 * Initialize the random number generator
 * Seeds from TSC and boot time for entropy
 */
void random_init(void);

/*
 * Get the next random 32-bit number
 * Uses LFSR (Linear Feedback Shift Register) algorithm
 * Returns a deterministic but seemingly random value
 */
uint32_t random_next(void);

/*
 * Get a random number in range [0, max)
 * Useful for bounded random selections
 */
uint32_t random_range(uint32_t max);

/*
 * Get a random number in range [min, max]
 * Inclusive on both ends
 */
uint32_t random_range_inclusive(uint32_t min, uint32_t max);

#endif /* DRIVERS_RANDOM_H */
