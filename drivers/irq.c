#include "irq.h"
#include "serial.h"
#include "apic.h"
#include "keyboard.h"
#include "ioapic.h"
#include "legacy_irq.h"
#include "../boot/idt.h"
#include "../lib/klog.h"
#include "../lib/kdiag.h"
#include "../sched/scheduler.h"
#include "../lib/cpu.h"
#include "../lib/io.h"
#include "../boot/kernel_panic.h"

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
struct irq_route_state {
    int via_ioapic;
    uint32_t gsi;
};

static struct irq_route_state irq_route_table[IRQ_LINES];
static int irq_system_initialized = 0;
static uint64_t timer_tick_counter = 0;
static uint64_t keyboard_event_counter = 0;

static inline int irq_line_has_ioapic_route(uint8_t irq) {
    if (irq >= IRQ_LINES) {
        return 0;
    }
    return irq_route_table[irq].via_ioapic;
}

static inline void acknowledge_irq(void) {
    apic_send_eoi();
}

static void mask_irq_line(uint8_t irq) {
    if (irq >= IRQ_LINES) {
        return;
    }

    if (!irq_table[irq].masked) {
        if (irq_line_has_ioapic_route(irq)) {
            ioapic_mask_gsi(irq_route_table[irq].gsi);
        } else {
            klog_printf(KLOG_INFO, "IRQ: Mask request ignored for line %u (no IOAPIC route)\n",
                        irq);
        }
        irq_table[irq].masked = 1;
    }
}

static void unmask_irq_line(uint8_t irq) {
    if (irq >= IRQ_LINES) {
        return;
    }

    if (!irq_table[irq].masked) {
        return;
    }

    if (irq_line_has_ioapic_route(irq)) {
        ioapic_unmask_gsi(irq_route_table[irq].gsi);
        irq_table[irq].masked = 0;
        return;
    }

    klog_printf(KLOG_INFO, "IRQ: Cannot unmask line %u (no IOAPIC route configured)\n", irq);
}

static void log_unhandled_irq(uint8_t irq, uint8_t vector) {
    if (irq >= IRQ_LINES) {
        klog_printf(KLOG_INFO, "IRQ: Spurious vector %u received\n", vector);
        return;
    }

    if (irq_table[irq].reported_unhandled) {
        return;
    }

    irq_table[irq].reported_unhandled = 1;

    klog_printf(KLOG_INFO, "IRQ: Unhandled IRQ %u (vector %u) - masking line\n", irq, vector);
}

static void timer_irq_handler(uint8_t irq, struct interrupt_frame *frame, void *context) {
    (void)irq;
    (void)frame;
    (void)context;

    timer_tick_counter++;

    if (timer_tick_counter <= 3) {
        klog_printf(KLOG_DEBUG, "IRQ: Timer tick #%llu\n",
                    (unsigned long long)timer_tick_counter);
    }

    scheduler_timer_tick();
}

static void keyboard_irq_handler(uint8_t irq, struct interrupt_frame *frame, void *context) {
    (void)irq;
    (void)frame;
    (void)context;

    uint8_t status = io_inb(PS2_STATUS_PORT);
    if (!(status & 0x01)) {
        return;
    }

    uint8_t scancode = io_inb(PS2_DATA_PORT);
    keyboard_event_counter++;

    /* Pass scancode to keyboard driver for processing */
    keyboard_handle_scancode(scancode);
}

static void irq_program_ioapic_route(uint8_t irq) {
    if (irq >= IRQ_LINES) {
        return;
    }

    if (!apic_is_enabled() || !ioapic_is_ready()) {
        kernel_panic("IRQ: APIC/IOAPIC unavailable during route programming");
    }

    uint32_t gsi = 0;
    uint32_t legacy_flags = 0;
    if (ioapic_legacy_irq_info(irq, &gsi, &legacy_flags) != 0) {
        kernel_panic("IRQ: Failed to translate legacy IRQ");
    }

    uint8_t vector = (uint8_t)(IRQ_BASE_VECTOR + irq);
    uint8_t lapic_id = (uint8_t)apic_get_id();
    uint32_t flags = IOAPIC_FLAG_DELIVERY_FIXED |
                     IOAPIC_FLAG_DEST_PHYSICAL |
                     legacy_flags |
                     IOAPIC_FLAG_MASK;

    if (ioapic_config_irq(gsi, vector, lapic_id, flags) != 0) {
        kernel_panic("IRQ: Failed to program IOAPIC route");
    }

    irq_route_table[irq].via_ioapic = 1;
    irq_route_table[irq].gsi = gsi;

    const char *polarity = (legacy_flags & IOAPIC_FLAG_POLARITY_LOW) ? "active-low" : "active-high";
    const char *trigger = (legacy_flags & IOAPIC_FLAG_TRIGGER_LEVEL) ? "level" : "edge";

    klog_printf(KLOG_INFO, "IRQ: IOAPIC route IRQ %u -> GSI %u, vector 0x%x (%s, %s)\n",
                irq, gsi, vector, polarity, trigger);

    if (irq_table[irq].masked) {
        ioapic_mask_gsi(gsi);
    } else {
        ioapic_unmask_gsi(gsi);
    }
}

static void irq_setup_ioapic_routes(void) {
    if (!apic_is_enabled() || !ioapic_is_ready()) {
        kernel_panic("IRQ: APIC/IOAPIC not ready during dispatcher init");
    }

    irq_program_ioapic_route(LEGACY_IRQ_TIMER);
    irq_program_ioapic_route(LEGACY_IRQ_KEYBOARD);
    irq_program_ioapic_route(LEGACY_IRQ_COM1);
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
        irq_route_table[i].via_ioapic = 0;
        irq_route_table[i].gsi = 0;
    }

    irq_system_initialized = 1;

    irq_setup_ioapic_routes();

    /* Initialize keyboard driver */
    keyboard_init();

    irq_register_handler(0, timer_irq_handler, NULL, "timer");
    irq_register_handler(1, keyboard_irq_handler, NULL, "keyboard");

    /* Enable interrupts globally once IDT/APIC/IOAPIC routes and handlers are ready. */
    cpu_sti();
}

int irq_register_handler(uint8_t irq, irq_handler_t handler, void *context, const char *name) {
    if (irq >= IRQ_LINES) {
        klog_info("IRQ: Attempted to register handler for invalid line");
        return -1;
    }

    if (handler == NULL) {
        klog_info("IRQ: Attempted to register NULL handler");
        return -1;
    }

    irq_table[irq].handler = handler;
    irq_table[irq].context = context;
    irq_table[irq].name = name;
    irq_table[irq].reported_unhandled = 0;

    if (name != NULL) {
        klog_printf(KLOG_DEBUG, "IRQ: Registered handler for line %u (%s)\n", irq, name);
    } else {
        klog_printf(KLOG_DEBUG, "IRQ: Registered handler for line %u\n", irq);
    }

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

    klog_printf(KLOG_DEBUG, "IRQ: Unregistered handler for line %u\n", irq);
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
        klog_info("IRQ: Received null frame");
        return;
    }

    uint8_t vector = (uint8_t)(frame->vector & 0xFF);
    const uint64_t expected_cs = frame->cs;
    const uint64_t expected_rip = frame->rip;

    if (!irq_system_initialized) {
        klog_info("IRQ: Dispatch received before initialization");
        if (vector >= IRQ_BASE_VECTOR) {
            acknowledge_irq();
        }
        return;
    }

    if (vector < IRQ_BASE_VECTOR) {
        klog_printf(KLOG_INFO, "IRQ: Received non-IRQ vector %u\n", vector);
        return;
    }

    uint8_t irq = vector - IRQ_BASE_VECTOR;

    if (irq >= IRQ_LINES) {
        log_unhandled_irq(0xFF, vector);
        acknowledge_irq();
        return;
    }

    struct irq_entry *entry = &irq_table[irq];

    if (!entry->handler) {
        log_unhandled_irq(irq, vector);
        mask_irq_line(irq);
        acknowledge_irq();
        return;
    }

    entry->count++;
    entry->last_timestamp = cpu_read_tsc();

    entry->handler(irq, frame, entry->context);

    if (frame->cs != expected_cs || frame->rip != expected_rip) {
        klog_printf(KLOG_INFO, "IRQ: Frame corruption detected on IRQ %u - aborting\n", irq);
        kdiag_dump_interrupt_frame(frame);
        kernel_panic("IRQ: frame corrupted");
    }

    acknowledge_irq();

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
