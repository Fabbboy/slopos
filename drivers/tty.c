#include "tty.h"
#include "keyboard.h"
#include "serial.h"

#include <stddef.h>
#include <stdint.h>

#include "../sched/scheduler.h"

/* ========================================================================
 * TTY INPUT BUFFER (Unified interrupt-driven input)
 * ======================================================================== */

#define TTY_INPUT_BUFFER_SIZE 256

typedef struct tty_input_buffer {
    char data[TTY_INPUT_BUFFER_SIZE];
    uint32_t head;      /* Write position (interrupt handlers write here) */
    uint32_t tail;      /* Read position (tty_read_line reads here) */
    uint32_t count;     /* Number of characters in buffer */
} tty_input_buffer_t;

static tty_input_buffer_t tty_input_buffer = {0};

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

/* ========================================================================
 * TTY INPUT BUFFER OPERATIONS
 * ======================================================================== */

/*
 * Check if TTY input buffer is full
 */
static inline int tty_buffer_full(void) {
    return tty_input_buffer.count >= TTY_INPUT_BUFFER_SIZE;
}

/*
 * Check if TTY input buffer is empty
 */
static inline int tty_buffer_empty(void) {
    return tty_input_buffer.count == 0;
}

/*
 * Push character to TTY input buffer (called from interrupt context)
 * Returns 0 on success, -1 if buffer is full
 */
static int tty_buffer_push(char c) {
    if (tty_buffer_full()) {
        /* Buffer full - drop oldest character (overwrite tail) */
        tty_input_buffer.tail = (tty_input_buffer.tail + 1) % TTY_INPUT_BUFFER_SIZE;
    } else {
        tty_input_buffer.count++;
    }

    tty_input_buffer.data[tty_input_buffer.head] = c;
    tty_input_buffer.head = (tty_input_buffer.head + 1) % TTY_INPUT_BUFFER_SIZE;

    return 0;
}

/*
 * Pop character from TTY input buffer (called from task context)
 * Returns character if available, 0 if buffer empty
 */
static char tty_buffer_pop(void) {
    tty_interrupts_disable();

    if (tty_buffer_empty()) {
        tty_interrupts_enable();
        return 0;
    }

    char c = tty_input_buffer.data[tty_input_buffer.tail];
    tty_input_buffer.tail = (tty_input_buffer.tail + 1) % TTY_INPUT_BUFFER_SIZE;
    tty_input_buffer.count--;

    tty_interrupts_enable();

    return c;
}

/*
 * Check if TTY buffer has data (non-destructive, safe to call without locks)
 */
static int tty_buffer_has_data(void) {
    tty_interrupts_disable();
    int has_data = tty_input_buffer.count > 0;
    tty_interrupts_enable();
    return has_data;
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

/*
 * Transfer available input from keyboard/serial to TTY buffer
 * Called from interrupt context
 */
static void tty_transfer_input_to_buffer(void) {
    /* Transfer from keyboard buffer */
    while (keyboard_has_input()) {
        char c = keyboard_getchar();
        tty_buffer_push(c);
    }

    /* Transfer from serial buffer */
    while (serial_data_available(SERIAL_COM1_PORT)) {
        char c = serial_getc(SERIAL_COM1_PORT);

        /* Normalize line endings and special characters */
        if (c == '\r') {
            c = '\n';
        } else if (c == 0x7F) {
            c = '\b';
        }

        tty_buffer_push(c);
    }
}

static int tty_input_available(void) {
    return tty_buffer_has_data();
}

static int tty_input_available_locked(void) {
    return tty_input_buffer.count > 0;
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
    /* Transfer input from keyboard/serial to TTY buffer */
    tty_transfer_input_to_buffer();

    if (!scheduler_is_enabled()) {
        return;
    }

    /* Wake up one waiting task if input is available */
    if (!tty_input_available()) {
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
 * Get next character from TTY input buffer
 * Returns character if available, 0 otherwise
 * This is interrupt-driven - data is populated by interrupt handlers
 */
static char tty_get_char(void) {
    /* Check if data is available in TTY buffer */
    if (!tty_buffer_has_data()) {
        return 0;
    }

    /* Read character from TTY buffer */
    return tty_buffer_pop();
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
        /* Block until input is available in the TTY buffer (interrupt-driven) */
        while (!tty_input_available()) {
            tty_block_until_input_ready();
        }

        /* Read character from TTY buffer (populated by interrupts) */
        char c = tty_get_char();
        if (c == 0) {
            /* Spurious wake-up or race condition, loop again */
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
