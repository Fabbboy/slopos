/*
 * Legacy PIC (8259) shutdown helpers
 * Masks and disables both PICs so the kernel can rely entirely on the APIC path.
 */

#include "pic_quiesce.h"

#include <stdint.h>

#define PIC1_COMMAND 0x20
#define PIC1_DATA    0x21
#define PIC2_COMMAND 0xA0
#define PIC2_DATA    0xA1

#define PIC_EOI      0x20

static inline void outb(uint16_t port, uint8_t value) {
    __asm__ volatile ("outb %0, %1" : : "a"(value), "Nd"(port));
}

void pic_quiesce_mask_all(void) {
    outb(PIC1_DATA, 0xFF);
    outb(PIC2_DATA, 0xFF);
}

void pic_quiesce_disable(void) {
    pic_quiesce_mask_all();
    /* Issue a spurious EOI to ensure no pending interrupts remain */
    outb(PIC1_COMMAND, PIC_EOI);
    outb(PIC2_COMMAND, PIC_EOI);
}
