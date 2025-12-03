#ifndef LIB_ALIGNMENT_H
#define LIB_ALIGNMENT_H

#include <stdint.h>

static inline uint64_t align_down_u64(uint64_t value, uint64_t alignment) {
    if (alignment == 0) {
        return value;
    }
    return value & ~(alignment - 1);
}

static inline uint64_t align_up_u64(uint64_t value, uint64_t alignment) {
    if (alignment == 0) {
        return value;
    }
    return (value + alignment - 1) & ~(alignment - 1);
}

#endif /* LIB_ALIGNMENT_H */

