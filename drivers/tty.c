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

static int tty_wait_queue_push(task_t *task) {
    if (!task || tty_wait_queue.count >= TTY_MAX_WAITERS) {
        return -1;
    }

    tty_wait_queue.tasks[tty_wait_queue.tail] = task;
    tty_wait_queue.tail = (tty_wait_queue.tail + 1) % TTY_MAX_WAITERS;
    tty_wait_queue.count++;
    return 0;
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
    if (keyboard_has_input()) {
        return 1;
    }

    if (serial_buffer_pending(SERIAL_COM1_PORT)) {
        return 1;
    }

    return 0;
}

static int tty_input_available_locked(void) {
    if (keyboard_buffer_pending()) {
        return 1;
    }

    if (serial_buffer_pending(SERIAL_COM1_PORT)) {
        return 1;
    }

    return 0;
}

static void tty_block_until_input_ready(void) {
    if (!scheduler_is_enabled()) {
        tty_cpu_relax();
        return;
    }

    task_t *current = task_get_current();
    if (!current) {
        tty_cpu_relax();
        return;
    }

    if (tty_input_available()) {
        return;
    }

    tty_interrupts_disable();

    if (tty_input_available_locked()) {
        tty_interrupts_enable();
        return;
    }

    if (tty_wait_queue_push(current) != 0) {
        tty_interrupts_enable();
        yield();
        return;
    }

    task_set_state(current->task_id, TASK_STATE_BLOCKED);
    unschedule_task(current);

    tty_interrupts_enable();

    schedule();
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

    if (keyboard_has_input()) {
        *out_char = keyboard_getchar();
        return 1;
    }

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
        if (!tty_dequeue_input_char(&c)) {
            tty_block_until_input_ready();
            continue;
        }

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
