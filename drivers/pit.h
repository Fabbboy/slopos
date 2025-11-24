#ifndef DRIVERS_PIT_H
#define DRIVERS_PIT_H

#include <stdint.h>

#define PIT_BASE_FREQUENCY_HZ 1193182U
#define PIT_DEFAULT_FREQUENCY_HZ 100U

void pit_init(uint32_t frequency_hz);
void pit_set_frequency(uint32_t frequency_hz);
uint32_t pit_get_frequency(void);
void pit_enable_irq(void);
void pit_disable_irq(void);
void pit_sleep_ms(uint32_t ms);

#endif /* DRIVERS_PIT_H */

