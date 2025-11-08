/*
 * SlopOS IOAPIC Driver Interface
 * Provides discovery and configuration of I/O APIC controllers
 */

#ifndef SLOPOS_IOAPIC_H
#define SLOPOS_IOAPIC_H

#include <stdint.h>

/* Redirection entry flag helpers */
#define IOAPIC_FLAG_DELIVERY_FIXED          (0u << 8)
#define IOAPIC_FLAG_DELIVERY_LOWEST_PRI     (1u << 8)
#define IOAPIC_FLAG_DELIVERY_SMI            (2u << 8)
#define IOAPIC_FLAG_DELIVERY_NMI            (4u << 8)
#define IOAPIC_FLAG_DELIVERY_INIT           (5u << 8)
#define IOAPIC_FLAG_DELIVERY_EXTINT         (7u << 8)

#define IOAPIC_FLAG_DEST_PHYSICAL           (0u << 11)
#define IOAPIC_FLAG_DEST_LOGICAL            (1u << 11)

#define IOAPIC_FLAG_POLARITY_HIGH           (0u << 13)
#define IOAPIC_FLAG_POLARITY_LOW            (1u << 13)

#define IOAPIC_FLAG_TRIGGER_EDGE            (0u << 15)
#define IOAPIC_FLAG_TRIGGER_LEVEL           (1u << 15)

#define IOAPIC_FLAG_MASK                    (1u << 16)
#define IOAPIC_FLAG_UNMASKED                0u

int ioapic_init(void);
int ioapic_config_irq(uint32_t gsi, uint8_t vector, uint8_t lapic_id, uint32_t flags);
int ioapic_mask_gsi(uint32_t gsi);
int ioapic_unmask_gsi(uint32_t gsi);
int ioapic_route_legacy_irq1(uint8_t vector);

#endif /* SLOPOS_IOAPIC_H */
