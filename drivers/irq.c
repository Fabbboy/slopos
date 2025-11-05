#include "irq.h"
#include "serial.h"
#include "pic.h"
#include "apic.h"
#include "keyboard.h"
#include "../boot/idt.h"
#include "../boot/log.h"
#include "../sched/scheduler.h"

#include <stddef.h>
#include <stdint.h>

#define IRQ_LINES 16
#define PS2_DATA_PORT 0x60
#define PS2_STATUS_PORT 0x64

struct irq_entry {
    irq_handler_t handler;
    void *context;
    const char *name;
    uint64_t count;
    uint64_t last_timestamp;
    int masked;
    int reported_unhandled;
};

static struct irq_entry irq_table[IRQ_LINES];
static int irq_system_initialized = 0;
static uint64_t timer_tick_counter = 0;
static uint64_t keyboard_event_counter = 0;

static inline uint64_t read_tsc(void) {
    uint32_t low, high;
    __asm__ volatile ("rdtsc" : "=a" (low), "=d" (high));
    return ((uint64_t)high << 32) | low;
}

static inline uint8_t inb(uint16_t port) {
    uint8_t value;
    __asm__ volatile ("inb %1, %0" : "=a"(value) : "Nd"(port));
    return value;
}

static inline void acknowledge_irq(uint8_t irq) {
    apic_send_eoi();

    if (!apic_is_enabled()) {
        if (irq < IRQ_LINES) {
            pic_send_eoi(irq);
        }
    }
}

static void mask_irq_line(uint8_t irq) {
    if (irq >= IRQ_LINES) {
        return;
    }

    if (!irq_table[irq].masked) {
        pic_disable_irq(irq);
        irq_table[irq].masked = 1;
    }
}

static void unmask_irq_line(uint8_t irq) {
    if (irq >= IRQ_LINES) {
        return;
    }

    if (irq_table[irq].masked) {
        pic_enable_irq(irq);
        irq_table[irq].masked = 0;
    }
}

static void log_unhandled_irq(uint8_t irq, uint8_t vector) {
    if (irq >= IRQ_LINES) {
        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_INFO, {
            kprint("IRQ: Spurious vector ");
            kprint_dec(vector);
            kprintln(" received");
        });
        return;
    }

    if (irq_table[irq].reported_unhandled) {
        return;
    }

    irq_table[irq].reported_unhandled = 1;

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_INFO, {
        kprint("IRQ: Unhandled IRQ ");
        kprint_dec(irq);
        kprint(" (vector ");
        kprint_dec(vector);
        kprintln(") - masking line");
    });
}

static void timer_irq_handler(uint8_t irq, struct interrupt_frame *frame, void *context) {
    (void)irq;
    (void)frame;
    (void)context;

    timer_tick_counter++;

    if (timer_tick_counter <= 3) {
        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
            kprint("IRQ: Timer tick #");
            kprint_dec(timer_tick_counter);
            kprintln("");
        });
    }

    scheduler_timer_tick();
}

static void keyboard_irq_handler(uint8_t irq, struct interrupt_frame *frame, void *context) {
    (void)irq;
    (void)frame;
    (void)context;

    uint8_t status = inb(PS2_STATUS_PORT);
    if (!(status & 0x01)) {
        return;
    }

    uint8_t scancode = inb(PS2_DATA_PORT);
    keyboard_event_counter++;

    /* Debug: Log first few keyboard interrupts to verify they're working */
    if (keyboard_event_counter <= 5) {
        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
            kprint("IRQ: Keyboard interrupt #");
            kprint_dec(keyboard_event_counter);
            kprint(" - scancode=");
            kprint_hex(scancode);
            kprintln("");
        });
    }

    /* Pass scancode to keyboard driver for processing */
    keyboard_handle_scancode(scancode);
}

static uint64_t serial_com2_event_counter = 0;
static uint64_t serial_com1_event_counter = 0;

static void serial_com2_irq_handler(uint8_t irq, struct interrupt_frame *frame, void *context) {
    (void)irq;
    (void)frame;
    (void)context;

    serial_com2_event_counter++;

    /* Debug: Log first few serial interrupts */
    if (serial_com2_event_counter <= 3) {
        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
            kprint("IRQ: COM2 serial interrupt #");
            kprint_dec(serial_com2_event_counter);
            kprintln("");
        });
    }

    /* Pass to serial driver for processing */
    serial_handle_interrupt(COM2_BASE);
}

static void serial_com1_irq_handler(uint8_t irq, struct interrupt_frame *frame, void *context) {
    (void)irq;
    (void)frame;
    (void)context;

    serial_com1_event_counter++;

    /* Debug: Log first few serial interrupts */
    if (serial_com1_event_counter <= 3) {
        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
            kprint("IRQ: COM1 serial interrupt #");
            kprint_dec(serial_com1_event_counter);
            kprintln("");
        });
    }

    /* Pass to serial driver for processing */
    serial_handle_interrupt(COM1_BASE);
}

uint64_t irq_get_timer_ticks(void) {
    return timer_tick_counter;
}

void irq_init(void) {
    for (int i = 0; i < IRQ_LINES; i++) {
        irq_table[i].handler = NULL;
        irq_table[i].context = NULL;
        irq_table[i].name = NULL;
        irq_table[i].count = 0;
        irq_table[i].last_timestamp = 0;
        irq_table[i].masked = 1;
        irq_table[i].reported_unhandled = 0;
    }

    irq_system_initialized = 1;

    /* Initialize keyboard driver */
    keyboard_init();

    /* Register interrupt handlers */
    irq_register_handler(0, timer_irq_handler, NULL, "timer");
    irq_register_handler(1, keyboard_irq_handler, NULL, "keyboard");
    irq_register_handler(3, serial_com2_irq_handler, NULL, "serial_com2");
    irq_register_handler(4, serial_com1_irq_handler, NULL, "serial_com1");

    pic_enable_safe_irqs();
}

int irq_register_handler(uint8_t irq, irq_handler_t handler, void *context, const char *name) {
    if (irq >= IRQ_LINES) {
        boot_log_info("IRQ: Attempted to register handler for invalid line");
        return -1;
    }

    if (handler == NULL) {
        boot_log_info("IRQ: Attempted to register NULL handler");
        return -1;
    }

    irq_table[irq].handler = handler;
    irq_table[irq].context = context;
    irq_table[irq].name = name;
    irq_table[irq].reported_unhandled = 0;

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("IRQ: Registered handler for line ");
        kprint_dec(irq);
        if (name != NULL) {
            kprint(" (");
            kprint(name);
            kprint(")");
        }
        kprintln("");
    });

    unmask_irq_line(irq);
    return 0;
}

void irq_unregister_handler(uint8_t irq) {
    if (irq >= IRQ_LINES) {
        return;
    }

    irq_table[irq].handler = NULL;
    irq_table[irq].context = NULL;
    irq_table[irq].name = NULL;
    irq_table[irq].reported_unhandled = 0;
    mask_irq_line(irq);

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("IRQ: Unregistered handler for line ");
        kprint_dec(irq);
        kprintln("");
    });
}

void irq_enable_line(uint8_t irq) {
    if (irq >= IRQ_LINES) {
        return;
    }

    irq_table[irq].reported_unhandled = 0;
    unmask_irq_line(irq);
}

void irq_disable_line(uint8_t irq) {
    if (irq >= IRQ_LINES) {
        return;
    }

    mask_irq_line(irq);
}

void irq_dispatch(struct interrupt_frame *frame) {
    if (!frame) {
        boot_log_info("IRQ: Received null frame");
        return;
    }

    uint8_t vector = (uint8_t)(frame->vector & 0xFF);

    if (!irq_system_initialized) {
        boot_log_info("IRQ: Dispatch received before initialization");
        if (vector >= IRQ_BASE_VECTOR) {
            uint8_t temp_irq = vector - IRQ_BASE_VECTOR;
            acknowledge_irq(temp_irq);
        }
        return;
    }

    if (vector < IRQ_BASE_VECTOR) {
        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_INFO, {
            kprint("IRQ: Received non-IRQ vector ");
            kprint_dec(vector);
            kprintln("");
        });
        return;
    }

    uint8_t irq = vector - IRQ_BASE_VECTOR;

    if (irq >= IRQ_LINES) {
        log_unhandled_irq(0xFF, vector);
        acknowledge_irq(irq);
        return;
    }

    struct irq_entry *entry = &irq_table[irq];

    if (!entry->handler) {
        log_unhandled_irq(irq, vector);
        mask_irq_line(irq);
        acknowledge_irq(irq);
        return;
    }

    entry->count++;
    entry->last_timestamp = read_tsc();

    entry->handler(irq, frame, entry->context);

    acknowledge_irq(irq);

    scheduler_handle_post_irq();
}

int irq_get_stats(uint8_t irq, struct irq_stats *out_stats) {
    if (irq >= IRQ_LINES || out_stats == NULL) {
        return -1;
    }

    out_stats->count = irq_table[irq].count;
    out_stats->last_timestamp = irq_table[irq].last_timestamp;
    return 0;
}
