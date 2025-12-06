#include "tty.h"
#include "keyboard.h"
#include "serial.h"

#include <stddef.h>
#include <stdint.h>

#include "../sched/scheduler.h"
#include "../lib/cpu.h"
#include "../lib/ring_buffer.h"

/* ========================================================================
 * WAIT QUEUE FOR BLOCKING INPUT
 * ======================================================================== */

#define TTY_MAX_WAITERS MAX_TASKS

typedef struct tty_wait_queue {
    task_t *tasks[TTY_MAX_WAITERS];
    size_t head;
    size_t tail;
    size_t count;
} tty_wait_queue_t;

static tty_wait_queue_t tty_wait_queue = {0};

static inline void tty_cpu_relax(void) {
    __asm__ volatile ("pause");
}

static inline void tty_service_serial_input(void) {
    serial_poll_receive(COM1_BASE);
}

static task_t *tty_wait_queue_pop(void) {
    task_t *task = NULL;
    int success = 0;
    RING_BUFFER_TRY_POP(&tty_wait_queue, tasks, &task, success);
    if (!success) {
        return NULL;
    }
    return task;
}

static int tty_input_available(void) {
    tty_service_serial_input();

    if (keyboard_has_input()) {
        return 1;
    }

    if (serial_buffer_pending(COM1_BASE)) {
        return 1;
    }

    return 0;
}

static void tty_block_until_input_ready(void) {
    /* Keep checking for input until available */
    while (1) {
        /* Check for input first */
        if (tty_input_available()) {
            break;
        }

        tty_service_serial_input();

        if (scheduler_is_enabled()) {
            /* Yield to other tasks while waiting */
            yield();
        } else {
            /* Fallback to CPU relaxation if scheduler not ready */
            tty_cpu_relax();
        }
    }
}

void tty_notify_input_ready(void) {
    if (!scheduler_is_enabled()) {
        return;
    }

    cpu_cli();

    task_t *task_to_wake = NULL;

    while (tty_wait_queue.count > 0) {
        task_t *candidate = tty_wait_queue_pop();
        if (!candidate) {
            continue;
        }

        if (!task_is_blocked(candidate)) {
            continue;
        }

        task_to_wake = candidate;
        break;
    }

    cpu_sti();

    if (task_to_wake) {
        if (unblock_task(task_to_wake) != 0) {
            /* Failed to unblock task; nothing else to do */
        }
    }
}

/* ========================================================================
 * HELPER FUNCTIONS
 * ======================================================================== */

/*
 * Check if character is printable (can be echoed)
 */
static inline int is_printable(char c) {
    return (c >= 0x20 && c <= 0x7E) || c == '\t';
}

/*
 * Check if character is a control character that needs special handling
 */
static inline int is_control_char(char c) {
    return (c >= 0x00 && c <= 0x1F) || c == 0x7F;
}

/*
 * Fetch a character from the input buffers if one is available.
 * Keyboard input is prioritized over serial to keep shell latency low.
 * Returns 1 when a character was written to out_char, 0 if nothing pending.
 */
static int tty_dequeue_input_char(char *out_char) {
    if (!out_char) {
        return 0;
    }

    tty_service_serial_input();

    if (keyboard_has_input()) {
        *out_char = keyboard_getchar();
        return 1;
    }

    tty_service_serial_input();

    char raw = 0;
    if (serial_buffer_read(COM1_BASE, &raw)) {
        if (raw == '\r') {
            raw = '\n';
        } else if (raw == 0x7F) {
            raw = '\b';
        }

        *out_char = raw;
        return 1;
    }

    return 0;
}

/* ========================================================================
 * TTY READLINE IMPLEMENTATION
 * ======================================================================== */

size_t tty_read_line(char *buffer, size_t buffer_size) {
    if (!buffer || buffer_size == 0) {
        return 0;
    }
    
    /* Ensure we have at least space for null terminator */
    if (buffer_size < 2) {
        buffer[0] = '\0';
        return 0;
    }
    
    size_t pos = 0;  /* Current position in buffer */
    size_t max_pos = buffer_size - 1;  /* Maximum position (leave room for null terminator) */
    
    /* Read characters until Enter is pressed */
    while (1) {
        /* Block until character is available from keyboard or serial */
        char c = 0;
        if (!tty_dequeue_input_char(&c)) {
            tty_block_until_input_ready();
            continue;
        }

        uint16_t port = COM1_BASE;

        /* Handle Enter key - finish line input */
        if (c == '\n' || c == '\r') {
            buffer[pos] = '\0';
            serial_putc(port, '\n');  /* Echo newline */
            return pos;
        }
        
        /* Handle Backspace */
        if (c == '\b') {
            if (pos > 0) {
                /* Remove character from buffer */
                pos--;
                
                /* Erase character visually: backspace, space, backspace */
                serial_putc(port, '\b');
                serial_putc(port, ' ');
                serial_putc(port, '\b');
            }
            /* If buffer is empty, ignore backspace (no character to delete) */
            continue;
        }
        
        /* Handle buffer overflow */
        if (pos >= max_pos) {
            /* Buffer full - ignore new characters (or could beep/alert) */
            continue;
        }
        
        /* Handle printable characters */
        if (is_printable(c)) {
            buffer[pos++] = c;
            serial_putc(port, c);  /* Echo character */
            continue;
        }
        
        /* Handle other control characters (ignore by default) */
        if (is_control_char(c)) {
            /* Don't echo control characters */
            continue;
        }
        
        /* For any other character, store and echo if it's in printable range */
        if (pos < max_pos) {
            buffer[pos++] = c;
            serial_putc(port, c);
        }
    }
}
