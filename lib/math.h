/*
 * SlopOS Math Utilities
 * Common math functions for kernel use
 */

#ifndef LIB_MATH_H
#define LIB_MATH_H

/*
 * Absolute value for integers
 */
static inline int abs(int x) {
    return x < 0 ? -x : x;
}

/*
 * Minimum of two integers
 */
static inline int min(int a, int b) {
    return a < b ? a : b;
}

/*
 * Maximum of two integers
 */
static inline int max(int a, int b) {
    return a > b ? a : b;
}

/*
 * Unsigned minimum
 */
static inline unsigned int umin(unsigned int a, unsigned int b) {
    return a < b ? a : b;
}

/*
 * Unsigned maximum
 */
static inline unsigned int umax(unsigned int a, unsigned int b) {
    return a > b ? a : b;
}

#endif /* LIB_MATH_H */

