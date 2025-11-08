/*
 * Minimal PIC helpers used solely to quiesce the 8259s once APIC takes over.
 */

#ifndef SLOPOS_PIC_QUIESCE_H
#define SLOPOS_PIC_QUIESCE_H

void pic_quiesce_mask_all(void);
void pic_quiesce_disable(void);

#endif /* SLOPOS_PIC_QUIESCE_H */
