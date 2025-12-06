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

/* CPUID feature flags (standard leaf 1) */
#define CPUID_FEAT_EDX_APIC           (1u << 9)   /* Local APIC present */
#define CPUID_FEAT_ECX_X2APIC         (1u << 21)  /* x2APIC mode available */

/* APIC MSRs */
#define MSR_APIC_BASE                 0x1B

/* APIC base register flags */
#define APIC_BASE_BSP                 (1u << 8)   /* Bootstrap Processor */
#define APIC_BASE_X2APIC              (1u << 10)  /* x2APIC mode enabled */
#define APIC_BASE_GLOBAL_ENABLE       (1u << 11)  /* APIC globally enabled */
#define APIC_BASE_ADDR_MASK           0xFFFFF000u /* Physical base address mask */

#endif /* BOOT_CPU_DEFS_H */

