/*
 * SlopOS GDT Definitions
 * Segment selectors and descriptor values for 64-bit mode.
 */

#ifndef BOOT_GDT_DEFS_H
#define BOOT_GDT_DEFS_H

/* GDT segment selectors */
#define GDT_NULL_SELECTOR             0x00     /* Null selector (required first entry) */
#define GDT_CODE_SELECTOR             0x08     /* Code segment selector */
#define GDT_DATA_SELECTOR             0x10     /* Data segment selector */
#define GDT_TSS_SELECTOR              0x18     /* Task State Segment selector */

/* GDT segment descriptor values for 64-bit mode */
#define GDT_NULL_DESCRIPTOR           0x0000000000000000ULL
#define GDT_CODE_DESCRIPTOR_64        0x00AF9A000000FFFFULL  /* 64-bit code segment */
#define GDT_DATA_DESCRIPTOR_64        0x00AF92000000FFFFULL  /* 64-bit data segment */

#endif /* BOOT_GDT_DEFS_H */

