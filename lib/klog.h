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
void klog(enum klog_level level, const char *msg);      /* Prints with newline */
void klog_raw(enum klog_level level, const char *msg);  /* Prints without newline */
void klog_hex(enum klog_level level, uint64_t value);
void klog_decimal(enum klog_level level, uint64_t value);
void klog_hex_byte(enum klog_level level, uint8_t value);

/* Convenience wrappers */
static inline void klog_error(const char *msg) { klog(KLOG_ERROR, msg); }
static inline void klog_warn(const char *msg)  { klog(KLOG_WARN, msg); }
static inline void klog_info(const char *msg)  { klog(KLOG_INFO, msg); }
static inline void klog_debug(const char *msg) { klog(KLOG_DEBUG, msg); }
static inline void klog_trace(const char *msg) { klog(KLOG_TRACE, msg); }

#define KLOG_BLOCK(level, code) \
    do { \
        if (klog_is_enabled(level)) { \
            code; \
        } \
    } while (0)

#endif /* LIB_KLOG_H */

