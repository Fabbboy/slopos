#ifndef LIB_RING_BUFFER_H
#define LIB_RING_BUFFER_H

#include <stdint.h>

/*
 * Simple fixed-size ring buffer helpers.
 * Expected struct layout:
 *   - array field named by caller (e.g. data, tasks, entries)
 *   - uint32_t head, tail, count fields
 *
 * These macros avoid repeating the same head/tail/count math across drivers.
 */

#define RING_BUFFER_CAPACITY(buf, field) \
    (uint32_t)(sizeof((buf)->field) / sizeof((buf)->field[0]))

#define RING_BUFFER_RESET(buf)        \
    do {                              \
        (buf)->head = 0;              \
        (buf)->tail = 0;              \
        (buf)->count = 0;             \
    } while (0)

#define RING_BUFFER_IS_EMPTY(buf) ((buf)->count == 0)
#define RING_BUFFER_IS_FULL(buf, field) \
    ((buf)->count >= RING_BUFFER_CAPACITY((buf), field))

/* Overwrite-oldest push (used for loss-tolerant RX buffers) */
#define RING_BUFFER_PUSH_OVERWRITE(buf, field, value)                          \
    do {                                                                       \
        if (RING_BUFFER_IS_FULL((buf), field)) {                               \
            (buf)->tail = ((buf)->tail + 1) % RING_BUFFER_CAPACITY((buf), field); \
            (buf)->count--;                                                    \
        }                                                                      \
        (buf)->field[(buf)->head] = (value);                                   \
        (buf)->head = ((buf)->head + 1) % RING_BUFFER_CAPACITY((buf), field);  \
        (buf)->count++;                                                        \
    } while (0)

/* Push that fails when full; success_var set to 1 on success, 0 on full */
#define RING_BUFFER_TRY_PUSH(buf, field, value, success_var)                   \
    do {                                                                       \
        if (RING_BUFFER_IS_FULL((buf), field)) {                               \
            success_var = 0;                                                   \
        } else {                                                               \
            (buf)->field[(buf)->head] = (value);                               \
            (buf)->head = ((buf)->head + 1) % RING_BUFFER_CAPACITY((buf), field); \
            (buf)->count++;                                                    \
            success_var = 1;                                                   \
        }                                                                      \
    } while (0)

/* Pop element; success_var set to 1 on success, 0 when empty */
#define RING_BUFFER_TRY_POP(buf, field, out_ptr, success_var)                  \
    do {                                                                       \
        if (RING_BUFFER_IS_EMPTY((buf))) {                                     \
            success_var = 0;                                                   \
        } else {                                                               \
            *(out_ptr) = (buf)->field[(buf)->tail];                            \
            (buf)->tail = ((buf)->tail + 1) % RING_BUFFER_CAPACITY((buf), field); \
            (buf)->count--;                                                    \
            success_var = 1;                                                   \
        }                                                                      \
    } while (0)

#endif /* LIB_RING_BUFFER_H */

