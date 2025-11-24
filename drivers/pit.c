#include "pit.h"
#include "serial.h"
#include "irq.h"
#include "../boot/log.h"
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
    __asm__ volatile ("outb %0, %1" : : "a" ((uint8_t)0), "Nd" ((uint16_t)0x80));
}

static inline void pit_outb(uint16_t port, uint8_t value) {
    __asm__ volatile ("outb %0, %1" : : "a" (value), "Nd" (port));
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

    pit_outb(PIT_COMMAND_PORT, PIT_COMMAND_CHANNEL0 |
                                  PIT_COMMAND_ACCESS_LOHI |
                                  PIT_COMMAND_MODE_SQUARE |
                                  PIT_COMMAND_BINARY);
    pit_outb(PIT_CHANNEL0_PORT, (uint8_t)(divisor & 0xFF));
    pit_outb(PIT_CHANNEL0_PORT, (uint8_t)((divisor >> 8) & 0xFF));
    pit_io_wait();

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("PIT: frequency set to ");
        kprint_dec(current_frequency_hz);
        kprintln(" Hz");
    });
}

void pit_init(uint32_t frequency_hz) {
    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_INFO, {
        kprint("PIT: Initializing timer at ");
        kprint_dec(frequency_hz ? frequency_hz : PIT_DEFAULT_FREQUENCY_HZ);
        kprintln(" Hz");
    });

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
