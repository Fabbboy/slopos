#include "numfmt.h"

#include <limits.h>

#include "memory.h"
#include "string.h"

static const char hex_digits[] = "0123456789ABCDEF";

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

size_t numfmt_u64_to_hex(uint64_t value, char *buffer, size_t buffer_len, int with_prefix) {
    if (!buffer || buffer_len == 0) {
        return 0;
    }

    /* 16 hex digits + optional 0x + null */
    size_t needed = 16 + (with_prefix ? 2 : 0) + 1;
    if (buffer_len < needed) {
        buffer[0] = '\0';
        return 0;
    }

    size_t pos = 0;
    if (with_prefix) {
        buffer[pos++] = '0';
        buffer[pos++] = 'x';
    }

    for (int i = 15; i >= 0; i--) {
        buffer[pos++] = hex_digits[(value >> (i * 4)) & 0xF];
    }

    buffer[pos] = '\0';
    return pos;
}

size_t numfmt_u8_to_hex(uint8_t value, char *buffer, size_t buffer_len) {
    if (!buffer || buffer_len < 3) {
        if (buffer_len > 0 && buffer) {
            buffer[0] = '\0';
        }
        return 0;
    }

    buffer[0] = hex_digits[(value >> 4) & 0xF];
    buffer[1] = hex_digits[value & 0xF];
    buffer[2] = '\0';
    return 2;
}

int numfmt_parse_u32(const char *str, uint32_t *out, uint32_t fallback) {
    if (!out) {
        return -1;
    }

    if (!str) {
        *out = fallback;
        return -1;
    }

    while (*str && isspace_k((int)*str)) {
        str++;
    }

    uint64_t value = 0;
    int seen_digit = 0;

    while (*str && isdigit_k((int)*str)) {
        seen_digit = 1;
        value = (value * 10u) + (uint64_t)(*str - '0');
        if (value > UINT32_MAX) {
            value = UINT32_MAX;
        }
        str++;
    }

    if (*str) {
        if ((*str == 'm' || *str == 'M') && (str[1] == 's' || str[1] == 'S')) {
            str += 2;
        }
    }

    while (*str && isspace_k((int)*str)) {
        str++;
    }

    if (!seen_digit || *str != '\0') {
        *out = fallback;
        return -1;
    }

    *out = (uint32_t)value;
    return 0;
}

int numfmt_parse_u64(const char *str, uint64_t *out, uint64_t fallback) {
    if (!out) {
        return -1;
    }

    if (!str) {
        *out = fallback;
        return -1;
    }

    while (*str && isspace_k((int)*str)) {
        str++;
    }

    uint64_t value = 0;
    int seen_digit = 0;

    while (*str && isdigit_k((int)*str)) {
        seen_digit = 1;
        if (value > (UINT64_MAX / 10u)) {
            value = UINT64_MAX;
        } else {
            value = (value * 10u) + (uint64_t)(*str - '0');
        }
        str++;
    }

    if (*str) {
        if ((*str == 'm' || *str == 'M') && (str[1] == 's' || str[1] == 'S')) {
            str += 2;
        }
    }

    while (*str && isspace_k((int)*str)) {
        str++;
    }

    if (!seen_digit || *str != '\0') {
        *out = fallback;
        return -1;
    }

    *out = value;
    return 0;
}

