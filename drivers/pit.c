#include "pit.h"
#include "serial.h"
#include "irq.h"
#include "../lib/klog.h"
#include "../lib/io.h"
#include <stdint.h>

#define PIT_CHANNEL0_PORT 0x40
#define PIT_COMMAND_PORT  0x43

#define PIT_COMMAND_CHANNEL0       0x00
#define PIT_COMMAND_ACCESS_LOHI    0x30
#define PIT_COMMAND_MODE_SQUARE    0x06
#define PIT_COMMAND_BINARY         0x00

#define PIT_IRQ_LINE 0

static uint32_t current_frequency_hz = 0;

static inline void pit_io_wait(void) {
    io_outb(0x80, 0);  /* I/O wait using port 0x80 */
}

static uint16_t pit_calculate_divisor(uint32_t frequency_hz) {
    if (frequency_hz == 0) {
        frequency_hz = PIT_DEFAULT_FREQUENCY_HZ;
    }

    if (frequency_hz > PIT_BASE_FREQUENCY_HZ) {
        frequency_hz = PIT_BASE_FREQUENCY_HZ;
    }

    uint32_t divisor = PIT_BASE_FREQUENCY_HZ / frequency_hz;
    if (divisor == 0) {
        divisor = 1;
    } else if (divisor > 0xFFFF) {
        divisor = 0xFFFF;
    }

    current_frequency_hz = PIT_BASE_FREQUENCY_HZ / divisor;
    return (uint16_t)divisor;
}

void pit_set_frequency(uint32_t frequency_hz) {
    uint16_t divisor = pit_calculate_divisor(frequency_hz);

    io_outb(PIT_COMMAND_PORT, PIT_COMMAND_CHANNEL0 |
                                  PIT_COMMAND_ACCESS_LOHI |
                                  PIT_COMMAND_MODE_SQUARE |
                                  PIT_COMMAND_BINARY);
    io_outb(PIT_CHANNEL0_PORT, (uint8_t)(divisor & 0xFF));
    io_outb(PIT_CHANNEL0_PORT, (uint8_t)((divisor >> 8) & 0xFF));
    pit_io_wait();

    klog_printf(KLOG_DEBUG, "PIT: frequency set to %u Hz\n", current_frequency_hz);
}

void pit_init(uint32_t frequency_hz) {
    klog_printf(KLOG_INFO, "PIT: Initializing timer at %u Hz\n",
                frequency_hz ? frequency_hz : PIT_DEFAULT_FREQUENCY_HZ);

    pit_set_frequency(frequency_hz ? frequency_hz : PIT_DEFAULT_FREQUENCY_HZ);

    irq_disable_line(PIT_IRQ_LINE);
}

uint32_t pit_get_frequency(void) {
    return current_frequency_hz ? current_frequency_hz : PIT_DEFAULT_FREQUENCY_HZ;
}

void pit_enable_irq(void) {
    irq_enable_line(PIT_IRQ_LINE);
}

void pit_disable_irq(void) {
    irq_disable_line(PIT_IRQ_LINE);
}

/* ========================================================================
 * DELAY FUNCTIONS
 * ======================================================================== */

/*
 * Read current PIT channel 0 counter value.
 * The counter counts DOWN from the divisor toward 0.
 */
static uint16_t pit_read_count(void) {
    /* Latch command: capture current count for channel 0 */
    io_outb(PIT_COMMAND_PORT, 0x00);
    
    /* Read low byte first, then high byte (as per PIT protocol) */
    uint8_t low = io_inb(PIT_CHANNEL0_PORT);
    uint8_t high = io_inb(PIT_CHANNEL0_PORT);
    
    return ((uint16_t)high << 8) | low;
}

/*
 * Polling-based delay using PIT counter (no interrupts required).
 * Reads the 1.193182 MHz counter directly for accurate timing.
 */
void pit_poll_delay_ms(uint32_t ms) {
    if (ms == 0) return;
    
    /* Calculate total ticks needed (1193 ticks ≈ 1ms) */
    uint32_t ticks_needed = (uint32_t)(((uint64_t)ms * PIT_BASE_FREQUENCY_HZ) / 1000);
    
    uint16_t last = pit_read_count();
    uint32_t elapsed = 0;
    
    while (elapsed < ticks_needed) {
        uint16_t current = pit_read_count();
        
        /* Counter counts DOWN, so last - current = forward progress */
        if (current <= last) {
            elapsed += (last - current);
        } else {
            /* Counter wrapped past zero */
            elapsed += last + (0xFFFF - current) + 1;
        }
        
        last = current;
    }
}

/*
 * IRQ-based sleep (requires interrupts to be enabled).
 * Uses HLT for power efficiency while waiting.
 */
void pit_sleep_ms(uint32_t ms) {
    if (ms == 0) return;

    /* Calculate target ticks */
    uint32_t freq = pit_get_frequency();
    uint64_t ticks_needed = (uint64_t)ms * freq / 1000;
    
    /* Handle case where ms is too small for 1 tick */
    if (ticks_needed == 0) ticks_needed = 1;

    uint64_t start_ticks = irq_get_timer_ticks();
    uint64_t target_ticks = start_ticks + ticks_needed;

    /* Wait loop */
    while (irq_get_timer_ticks() < target_ticks) {
        __asm__ volatile ("hlt");
    }
}
