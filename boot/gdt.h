/*
 * SlopOS Global Descriptor Table (GDT) and Task State Segment (TSS) setup
 * Provides APIs for initializing segmentation and configuring IST stacks
 */

#ifndef GDT_H
#define GDT_H

#include <stdint.h>

/* Initialize kernel GDT and load TSS */
void gdt_init(void);

/* Configure Interrupt Stack Table entry (1-based index) */
void gdt_set_ist(uint8_t index, uint64_t stack_top);

/* Boot stack symbol exported from assembly */
extern uint8_t kernel_stack_top;

#endif /* GDT_H */
