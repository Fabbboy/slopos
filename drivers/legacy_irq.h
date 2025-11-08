/*
 * Legacy IRQ index definitions for devices wired to the original PIC pins.
 * These values match the historical PIC numbering so IOAPIC routing logic
 * can translate legacy IRQ identifiers to GSIs without depending on PIC code.
 */

#ifndef SLOPOS_LEGACY_IRQ_H
#define SLOPOS_LEGACY_IRQ_H

#define LEGACY_IRQ_TIMER            0
#define LEGACY_IRQ_KEYBOARD         1
#define LEGACY_IRQ_CASCADE          2
#define LEGACY_IRQ_COM2             3
#define LEGACY_IRQ_COM1             4
#define LEGACY_IRQ_LPT2             5
#define LEGACY_IRQ_FLOPPY           6
#define LEGACY_IRQ_LPT1             7
#define LEGACY_IRQ_RTC              8
#define LEGACY_IRQ_RESERVED1        9
#define LEGACY_IRQ_RESERVED2        10
#define LEGACY_IRQ_RESERVED3        11
#define LEGACY_IRQ_MOUSE            12
#define LEGACY_IRQ_FPU              13
#define LEGACY_IRQ_ATA_PRIMARY      14
#define LEGACY_IRQ_ATA_SECONDARY    15

#endif /* SLOPOS_LEGACY_IRQ_H */
