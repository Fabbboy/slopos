/*
 * SlopOS GDT Definitions
 * Segment selectors and descriptor values for 64-bit mode.
 */

#ifndef BOOT_GDT_DEFS_H
#define BOOT_GDT_DEFS_H

/* GDT segment selectors */
#define GDT_NULL_SELECTOR             0x00     /* Null selector (required first entry) */
#define GDT_CODE_SELECTOR             0x08     /* Kernel code segment selector (RPL0) */
#define GDT_DATA_SELECTOR             0x10     /* Kernel data segment selector (RPL0) */
#define GDT_USER_DATA_SELECTOR        0x1B     /* User data segment selector (RPL3) */
#define GDT_USER_CODE_SELECTOR        0x23     /* User code segment selector (RPL3) */
#define GDT_TSS_SELECTOR              0x28     /* Task State Segment selector */

/* GDT segment descriptor values for 64-bit mode */
#define GDT_NULL_DESCRIPTOR           0x0000000000000000ULL
#define GDT_CODE_DESCRIPTOR_64        0x00AF9A000000FFFFULL  /* 64-bit kernel code segment */
#define GDT_DATA_DESCRIPTOR_64        0x00AF92000000FFFFULL  /* 64-bit kernel data segment */
#define GDT_USER_CODE_DESCRIPTOR_64   0x00AFFA000000FFFFULL  /* 64-bit user code segment (DPL=3) */
#define GDT_USER_DATA_DESCRIPTOR_64   0x00AFF2000000FFFFULL  /* 64-bit user data segment (DPL=3) */

#endif /* BOOT_GDT_DEFS_H */

