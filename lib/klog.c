/*
 * Unified kernel logging with early-boot support.
 * Falls back to raw COM1 writes until the serial driver is attached.
 */

#include "klog.h"
#include "../drivers/serial.h"
#include "../lib/io.h"
#include "../lib/numfmt.h"
#include "../lib/string.h"
#include <stdarg.h>

static enum klog_level current_level = KLOG_INFO;
static int serial_ready = 0;

static void klog_early_putc(char c);

static inline void klog_putc_internal(char c) {
    if (serial_ready) {
        serial_putc(COM1_BASE, c);
    } else {
        klog_early_putc(c);
    }
}

static void klog_early_putc(char c) {
    io_outb(COM1_BASE, c);
}

static void klog_emit(const char *text) {
    if (!text) {
        return;
    }

    if (serial_ready) {
        serial_puts(COM1_BASE, text);
        return;
    }

    const char *p = text;
    while (*p) {
        klog_early_putc(*p++);
    }
}

static void klog_emit_line(const char *text) {
    if (text) {
        klog_emit(text);
    }
    if (serial_ready) {
        serial_putc(COM1_BASE, '\n');
    } else {
        klog_early_putc('\n');
    }
}

void klog_init(void) {
    current_level = KLOG_INFO;
    serial_ready = 0;
}

void klog_attach_serial(void) {
    serial_ready = 1;
}

void klog_set_level(enum klog_level level) {
    current_level = level;
}

enum klog_level klog_get_level(void) {
    return current_level;
}

int klog_is_enabled(enum klog_level level) {
    return level <= current_level;
}

void klog_newline(void) {
    klog_emit_line(NULL);
}

/* ========================================================================
 * FORMATTED LOGGING
 * ======================================================================== */

static void klog_write_bytes(const char *str, size_t len) {
    if (!str) {
        return;
    }
    for (size_t i = 0; i < len; i++) {
        klog_putc_internal(str[i]);
    }
}

static void klog_write_padded(const char *str, size_t len, int width, int zero_pad) {
    char pad_char = zero_pad ? '0' : ' ';
    int padding = (width > 0 && (size_t)width > len) ? (width - (int)len) : 0;

    for (int i = 0; i < padding; i++) {
        klog_putc_internal(pad_char);
    }

    klog_write_bytes(str, len);
}

void klog_printf(enum klog_level level, const char *fmt, ...) {
    if (!klog_is_enabled(level) || !fmt) {
        return;
    }

    va_list ap;
    va_start(ap, fmt);

    while (*fmt) {
        if (*fmt != '%') {
            klog_putc_internal(*fmt++);
            continue;
        }

        fmt++; /* Skip '%' */
        if (*fmt == '%') {
            klog_putc_internal('%');
            fmt++;
            continue;
        }

        int zero_pad = 0;
        int width = 0;

        if (*fmt == '0') {
            zero_pad = 1;
            fmt++;
        }

        while (isdigit_k((int)*fmt)) {
            width = width * 10 + (*fmt - '0');
            fmt++;
        }

        enum {
            LEN_DEFAULT = 0,
            LEN_LONG,
            LEN_LLONG,
            LEN_SIZE
        } length = LEN_DEFAULT;

        if (*fmt == 'l') {
            fmt++;
            if (*fmt == 'l') {
                length = LEN_LLONG;
                fmt++;
            } else {
                length = LEN_LONG;
            }
        } else if (*fmt == 'z') {
            length = LEN_SIZE;
            fmt++;
        }

        char spec = *fmt++;
        char buffer[48];

        switch (spec) {
            case 's': {
                const char *str = va_arg(ap, const char *);
                if (!str) {
                    str = "(null)";
                }
                size_t len = strlen(str);
                klog_write_padded(str, len, width, 0);
                break;
            }
            case 'c': {
                char c = (char)va_arg(ap, int);
                char tmp[1] = { c };
                klog_write_padded(tmp, 1, width, zero_pad);
                break;
            }
            case 'd':
            case 'i': {
                long long value = 0;
                if (length == LEN_LLONG) {
                    value = va_arg(ap, long long);
                } else if (length == LEN_LONG) {
                    value = va_arg(ap, long);
                } else if (length == LEN_SIZE) {
                    value = (long long)va_arg(ap, size_t);
                } else {
                    value = va_arg(ap, int);
                }

                int negative = value < 0;
                uint64_t magnitude = negative ? (uint64_t)(-value) : (uint64_t)value;
                size_t digits = numfmt_u64_to_decimal(magnitude, buffer, sizeof(buffer));
                if (digits == 0) {
                    buffer[0] = '0';
                    buffer[1] = '\0';
                    digits = 1;
                }

                size_t total = digits + (negative ? 1 : 0);
                char pad_char = zero_pad ? '0' : ' ';
                int padding = (width > 0 && (size_t)width > total) ? (width - (int)total) : 0;

                if (negative && pad_char == '0') {
                    klog_putc_internal('-');
                    negative = 0; /* Sign already emitted */
                }

                for (int i = 0; i < padding; i++) {
                    klog_putc_internal(pad_char);
                }

                if (negative) {
                    klog_putc_internal('-');
                }

                klog_write_bytes(buffer, digits);
                break;
            }
            case 'u':
            case 'x':
            case 'X': {
                uint64_t value = 0;
                if (length == LEN_LLONG) {
                    value = va_arg(ap, unsigned long long);
                } else if (length == LEN_LONG) {
                    value = va_arg(ap, unsigned long);
                } else if (length == LEN_SIZE) {
                    value = va_arg(ap, size_t);
                } else {
                    value = va_arg(ap, unsigned int);
                }

                size_t len = 0;
                if (spec == 'u') {
                    len = numfmt_u64_to_decimal(value, buffer, sizeof(buffer));
                } else {
                    len = numfmt_u64_to_hex(value, buffer, sizeof(buffer), 0);
                    if (spec == 'x') {
                        for (size_t i = 0; i < len; i++) {
                            if (buffer[i] >= 'A' && buffer[i] <= 'F') {
                                buffer[i] = (char)(buffer[i] - 'A' + 'a');
                            }
                        }
                    }
                }

                if (len == 0) {
                    buffer[0] = '0';
                    buffer[1] = '\0';
                    len = 1;
                }

                klog_write_padded(buffer, len, width, zero_pad);
                break;
            }
            case 'p': {
                uint64_t ptr_val = (uint64_t)(uintptr_t)va_arg(ap, void *);
                size_t len = numfmt_u64_to_hex(ptr_val, buffer, sizeof(buffer), 1);
                if (len == 0) {
                    buffer[0] = '0';
                    buffer[1] = 'x';
                    buffer[2] = '0';
                    buffer[3] = '\0';
                    len = 3;
                }
                klog_write_padded(buffer, len, width, zero_pad);
                break;
            }
            default:
                /* Unknown specifier, print it literally */
                klog_putc_internal('%');
                if (spec != '\0') {
                    klog_putc_internal(spec);
                } else {
                    fmt--;
                }
                break;
        }
    }

    va_end(ap);
}

