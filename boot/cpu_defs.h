/*
 * SlopOS CPU Control Register and CPUID Definitions
 */

#ifndef BOOT_CPU_DEFS_H
#define BOOT_CPU_DEFS_H

/* EFLAGS bits */
#define EFLAGS_ID_BIT                 0x00200000  /* ID bit for CPUID detection */

/* CR0 bits */
#define CR0_PG_BIT                    0x80000000  /* Paging enable (bit 31) */
#define CR0_PE_BIT                    0x00000001  /* Protection enable (bit 0) */

/* CR4 bits */
#define CR4_PAE_BIT                   0x00000020  /* Physical Address Extension (bit 5) */

/* EFER MSR */
#define EFER_MSR                      0xC0000080  /* Extended Feature Enable Register */
#define EFER_LME_BIT                  0x00000100  /* Long Mode Enable (bit 8) */

/* CPUID function numbers */
#define CPUID_EXTENDED_FEATURES       0x80000000  /* Highest extended function */
#define CPUID_EXTENDED_FEATURE_INFO   0x80000001  /* Extended feature information */
#define CPUID_LONG_MODE_BIT           0x20000000  /* Long mode bit in EDX (bit 29) */

#endif /* BOOT_CPU_DEFS_H */

