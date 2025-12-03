#include "numfmt.h"

#include <limits.h>

#include "memory.h"

size_t numfmt_u64_to_decimal(uint64_t value, char *buffer, size_t buffer_len) {
    if (!buffer || buffer_len == 0) {
        return 0;
    }

    size_t write_pos = buffer_len - 1;
    buffer[write_pos] = '\0';

    if (value == 0) {
        if (write_pos == 0) {
            buffer[0] = '\0';
            return 0;
        }
        buffer[--write_pos] = '0';
    } else {
        while (value > 0) {
            if (write_pos == 0) {
                buffer[0] = '\0';
                return 0;
            }
            buffer[--write_pos] = (char)('0' + (value % 10));
            value /= 10;
        }
    }

    size_t len = (buffer_len - 1) - write_pos;
    memmove(buffer, buffer + write_pos, len + 1);
    return len;
}

size_t numfmt_i64_to_decimal(int64_t value, char *buffer, size_t buffer_len) {
    if (!buffer || buffer_len == 0) {
        return 0;
    }

    if (value >= 0) {
        return numfmt_u64_to_decimal((uint64_t)value, buffer, buffer_len);
    }

    if (buffer_len < 2) {
        buffer[0] = '\0';
        return 0;
    }

    buffer[0] = '-';

    uint64_t magnitude;
    if (value == INT64_MIN) {
        magnitude = ((uint64_t)INT64_MAX) + 1;
    } else {
        magnitude = (uint64_t)(-value);
    }

    size_t len = numfmt_u64_to_decimal(magnitude, buffer + 1, buffer_len - 1);
    if (len == 0) {
        buffer[0] = '\0';
        return 0;
    }

    return len + 1;
}

