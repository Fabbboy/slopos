#include "tty.h"
#include "keyboard.h"
#include "serial.h"

#include <stddef.h>
#include <stdint.h>

#include "../sched/scheduler.h"

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

static inline void tty_interrupts_disable(void) {
    __asm__ volatile ("cli" : : : "memory");
}

static inline void tty_interrupts_enable(void) {
    __asm__ volatile ("sti" : : : "memory");
}

static inline void tty_cpu_relax(void) {
    __asm__ volatile ("pause");
}

static inline void tty_service_serial_input(void) {
    serial_poll_receive(SERIAL_COM1_PORT);
}

static task_t *tty_wait_queue_pop(void) {
    if (tty_wait_queue.count == 0) {
        return NULL;
    }

    task_t *task = tty_wait_queue.tasks[tty_wait_queue.head];
    tty_wait_queue.tasks[tty_wait_queue.head] = NULL;
    tty_wait_queue.head = (tty_wait_queue.head + 1) % TTY_MAX_WAITERS;
    tty_wait_queue.count--;
    return task;
}

static int tty_input_available(void) {
    tty_service_serial_input();

    int kbd_has_input = keyboard_has_input();
    kprint("[TTY] keyboard_has_input() = ");
    kprint_decimal((uint64_t)kbd_has_input);
    kprint("\n");

    if (kbd_has_input) {
        return 1;
    }

    if (serial_buffer_pending(SERIAL_COM1_PORT)) {
        return 1;
    }

    return 0;
}

static void tty_block_until_input_ready(void) {
    /* Keep checking for input until available */
    while (1) {
        /* Check for input first */
        if (tty_input_available()) {
            kprint("[TTY] Input now available, breaking from block\n");
            break;
        }

        tty_service_serial_input();

        if (scheduler_is_enabled()) {
            /* Yield to other tasks while waiting */
            kprint("[TTY] Yielding to scheduler...\n");
            yield();
            kprint("[TTY] Resumed from yield, rechecking...\n");
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

    tty_interrupts_disable();

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

    tty_interrupts_enable();

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
    if (serial_buffer_read(SERIAL_COM1_PORT, &raw)) {
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
        kprint("[TTY] Checking for input...\n");
        if (!tty_dequeue_input_char(&c)) {
            kprint("[TTY] No input available, blocking...\n");
            tty_block_until_input_ready();
            continue;
        }

        kprint("[TTY] Got character: 0x");
        kprint_hex((uint64_t)c);
        kprint("\n");

        /* Handle Enter key - finish line input */
        if (c == '\n' || c == '\r') {
            buffer[pos] = '\0';
            kprint_char('\n');  /* Echo newline */
            return pos;
        }
        
        /* Handle Backspace */
        if (c == '\b') {
            if (pos > 0) {
                /* Remove character from buffer */
                pos--;
                
                /* Erase character visually: backspace, space, backspace */
                kprint_char('\b');
                kprint_char(' ');
                kprint_char('\b');
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
            kprint_char(c);  /* Echo character */
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
            kprint_char(c);
        }
    }
}
