#ifndef LIB_KLOG_H
#define LIB_KLOG_H

#include <stdint.h>
#include <stddef.h>

/*
 * Unified kernel logging with early-boot support.
 * Provides a single entry point for all kernel logging levels.
 */

enum klog_level {
    KLOG_ERROR = 0,
    KLOG_WARN  = 1,
    KLOG_INFO  = 2,
    KLOG_DEBUG = 3,
    KLOG_TRACE = 4,
};

void klog_init(void);
void klog_attach_serial(void);
void klog_set_level(enum klog_level level);
enum klog_level klog_get_level(void);
int klog_is_enabled(enum klog_level level);

void klog_newline(void);
void klog(enum klog_level level, const char *msg) __attribute__((deprecated("Use klog_printf instead")));      /* Prints with newline */
void klog_raw(enum klog_level level, const char *msg) __attribute__((deprecated("Use klog_printf instead")));  /* Prints without newline */
void klog_hex(enum klog_level level, uint64_t value) __attribute__((deprecated("Use klog_printf instead")));
void klog_decimal(enum klog_level level, uint64_t value) __attribute__((deprecated("Use klog_printf instead")));
void klog_hex_byte(enum klog_level level, uint8_t value) __attribute__((deprecated("Use klog_printf instead")));
void klog_printf(enum klog_level level, const char *fmt, ...)
    __attribute__((format(printf, 2, 3)));

/* Convenience wrappers */
static inline void klog_error(const char *msg) { klog_printf(KLOG_ERROR, "%s\n", msg); }
static inline void klog_warn(const char *msg)  { klog_printf(KLOG_WARN, "%s\n", msg); }
static inline void klog_info(const char *msg)  { klog_printf(KLOG_INFO, "%s\n", msg); }
static inline void klog_debug(const char *msg) { klog_printf(KLOG_DEBUG, "%s\n", msg); }
static inline void klog_trace(const char *msg) { klog_printf(KLOG_TRACE, "%s\n", msg); }

#define KLOG_BLOCK(level, code)                                     \
    do {                                                            \
        enum klog_level __klog_block_level = (level);               \
        if (klog_is_enabled(__klog_block_level)) {                  \
            (void)__klog_block_level;                               \
            code;                                                   \
        }                                                           \
    } while (0)

#endif /* LIB_KLOG_H */

