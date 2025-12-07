/*
 * SlopOS GDT Definitions
 * Segment selectors and descriptor values for 64-bit mode.
 *
 * PRIVILEGE LEVEL ENCODING:
 * The lowest 2 bits of segment selectors encode the Requested Privilege Level (RPL):
 *  - RPL=00b (0x0): Ring 0 (kernel mode)
 *  - RPL=11b (0x3): Ring 3 (user mode)
 *
 * Selectors ending in 0x8/0x0 are Ring 0, selectors ending in 0xB/0x3 are Ring 3.
 *
 * The segment descriptors encode the Descriptor Privilege Level (DPL) in bits 45-46:
 *  - DPL=0 (kernel segments): Only accessible from Ring 0-2
 *  - DPL=3 (user segments): Accessible from any privilege level
 *
 * The CPU enforces that CS.DPL must equal the Current Privilege Level (CPL), while
 * data segment DPLs must be ≥ CPL. This prevents user code from loading kernel segments.
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

