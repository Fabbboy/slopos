#ifndef LIB_NUMFMT_H
#define LIB_NUMFMT_H

#include <stddef.h>
#include <stdint.h>

size_t numfmt_u64_to_decimal(uint64_t value, char *buffer, size_t buffer_len);
size_t numfmt_i64_to_decimal(int64_t value, char *buffer, size_t buffer_len);
size_t numfmt_u64_to_hex(uint64_t value, char *buffer, size_t buffer_len, int with_prefix);
size_t numfmt_u8_to_hex(uint8_t value, char *buffer, size_t buffer_len);

#endif /* LIB_NUMFMT_H */

