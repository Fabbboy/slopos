#ifndef DRIVERS_PIT_H
#define DRIVERS_PIT_H

#include <stdint.h>

#define PIT_BASE_FREQUENCY_HZ 1193182U
#define PIT_DEFAULT_FREQUENCY_HZ 100U

/* Initialization and configuration */
void pit_init(uint32_t frequency_hz);
void pit_set_frequency(uint32_t frequency_hz);
uint32_t pit_get_frequency(void);

/* IRQ control */
void pit_enable_irq(void);
void pit_disable_irq(void);

/* Polling delay - works without interrupts (early boot) */
void pit_poll_delay_ms(uint32_t ms);

/* IRQ-based delay - requires interrupts enabled (normal operation) */
void pit_sleep_ms(uint32_t ms);

#endif /* DRIVERS_PIT_H */

