/*
 * Unified kernel logging with early-boot support.
 * Falls back to raw COM1 writes until the serial driver is attached.
 */

#include "klog.h"
#include "../boot/constants.h"
#include "../drivers/serial.h"
#include "../lib/io.h"
#include "../lib/numfmt.h"

static enum klog_level current_level = KLOG_INFO;
static int serial_ready = 0;

static const char hex_digits[] = "0123456789ABCDEF";

static void klog_early_putc(char c) {
    io_outb(COM1_BASE, c);
}

static void klog_emit(const char *text) {
    if (!text) {
        return;
    }

    if (serial_ready) {
        kprint(text);
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
        kprint("\n");
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

void klog(enum klog_level level, const char *msg) {
    if (!klog_is_enabled(level)) {
        return;
    }
    klog_emit_line(msg);
}

void klog_raw(enum klog_level level, const char *msg) {
    if (!klog_is_enabled(level) || !msg) {
        return;
    }
    klog_emit(msg);
}

void klog_hex(enum klog_level level, uint64_t value) {
    if (!klog_is_enabled(level)) {
        return;
    }

    char buffer[19];
    buffer[0] = '0';
    buffer[1] = 'x';
    for (int i = 0; i < 16; i++) {
        buffer[2 + i] = hex_digits[(value >> ((15 - i) * 4)) & 0xF];
    }
    buffer[18] = '\0';
    klog_raw(level, buffer);
}

void klog_decimal(enum klog_level level, uint64_t value) {
    if (!klog_is_enabled(level)) {
        return;
    }

    char buffer[32];
    if (numfmt_u64_to_decimal(value, buffer, sizeof(buffer)) == 0) {
        buffer[0] = '0';
        buffer[1] = '\0';
    }
    klog_raw(level, buffer);
}

