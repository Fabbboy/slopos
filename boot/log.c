/*
 * Boot logging implementation
 * Handles early boot output before the full debug infrastructure is online.
 */

#include "log.h"
#include "constants.h"
#include "../drivers/serial.h"
#include "../lib/io.h"

static enum boot_log_level current_level = BOOT_LOG_LEVEL_INFO;
static int serial_ready = 0;

static void boot_log_early_putc(char c) {
    io_outb(COM1_BASE, c);
}

static void boot_log_emit(const char *text) {
    if (!text) {
        return;
    }

    if (serial_ready) {
        kprint(text);
        return;
    }

    const char *p = text;
    while (*p) {
        boot_log_early_putc(*p++);
    }
}

static void boot_log_emit_line(const char *text) {
    if (text) {
        boot_log_emit(text);
    }
    if (serial_ready) {
        kprint("\n");
    } else {
        boot_log_early_putc('\n');
    }
}

void boot_log_set_level(enum boot_log_level level) {
    current_level = level;
}

enum boot_log_level boot_log_get_level(void) {
    return current_level;
}

int boot_log_is_enabled(enum boot_log_level level) {
    return level <= current_level;
}

void boot_log_attach_serial(void) {
    serial_ready = 1;
}

void boot_log_line(enum boot_log_level level, const char *text) {
    if (!boot_log_is_enabled(level)) {
        return;
    }
    boot_log_emit_line(text);
}

void boot_log_raw(enum boot_log_level level, const char *text) {
    if (!boot_log_is_enabled(level) || !text) {
        return;
    }
    boot_log_emit(text);
}

void boot_log_error(const char *text) {
    boot_log_line(BOOT_LOG_LEVEL_ERROR, text);
}

void boot_log_info(const char *text) {
    boot_log_line(BOOT_LOG_LEVEL_INFO, text);
}

void boot_log_debug(const char *text) {
    boot_log_line(BOOT_LOG_LEVEL_DEBUG, text);
}

void boot_log_newline(void) {
    boot_log_emit_line(NULL);
}
