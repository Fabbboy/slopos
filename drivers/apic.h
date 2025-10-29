/*
 * SlopOS APIC (Advanced Programmable Interrupt Controller) Driver
 * Local APIC and I/O APIC detection and basic initialization
 */

#ifndef APIC_H
#define APIC_H

#include <stdint.h>

// CPUID feature flags for APIC detection
#define CPUID_FEAT_EDX_APIC     (1 << 9)   // Local APIC present
#define CPUID_FEAT_ECX_X2APIC   (1 << 21)  // x2APIC mode available

// MSR addresses for APIC
#define MSR_APIC_BASE           0x1B
#define MSR_X2APIC_APICID       0x802
#define MSR_X2APIC_VERSION      0x803
#define MSR_X2APIC_LVT_TIMER    0x832
#define MSR_X2APIC_LVT_LINT0    0x835
#define MSR_X2APIC_LVT_LINT1    0x836
#define MSR_X2APIC_LVT_ERROR    0x837
#define MSR_X2APIC_SPURIOUS     0x80F

// APIC base register flags
#define APIC_BASE_BSP           (1 << 8)   // Bootstrap Processor
#define APIC_BASE_X2APIC        (1 << 10)  // x2APIC mode enabled
#define APIC_BASE_GLOBAL_ENABLE (1 << 11)  // APIC globally enabled
#define APIC_BASE_ADDR_MASK     0xFFFFF000 // Physical base address mask

// Local APIC register offsets (for memory-mapped APIC)
#define LAPIC_ID                0x020
#define LAPIC_VERSION           0x030
#define LAPIC_TPR               0x080  // Task Priority Register
#define LAPIC_APR               0x090  // Arbitration Priority Register
#define LAPIC_PPR               0x0A0  // Processor Priority Register
#define LAPIC_EOI               0x0B0  // End of Interrupt
#define LAPIC_RRD               0x0C0  // Remote Read Register
#define LAPIC_LDR               0x0D0  // Logical Destination Register
#define LAPIC_DFR               0x0E0  // Destination Format Register
#define LAPIC_SPURIOUS          0x0F0  // Spurious Interrupt Vector
#define LAPIC_ESR               0x280  // Error Status Register
#define LAPIC_ICR_LOW           0x300  // Interrupt Command Register (low)
#define LAPIC_ICR_HIGH          0x310  // Interrupt Command Register (high)
#define LAPIC_LVT_TIMER         0x320  // Timer Local Vector Table
#define LAPIC_LVT_THERMAL       0x330  // Thermal Local Vector Table
#define LAPIC_LVT_PERFCNT       0x340  // Performance Counter Local Vector Table
#define LAPIC_LVT_LINT0         0x350  // Local Interrupt 0 Local Vector Table
#define LAPIC_LVT_LINT1         0x360  // Local Interrupt 1 Local Vector Table
#define LAPIC_LVT_ERROR         0x370  // Error Local Vector Table
#define LAPIC_TIMER_ICR         0x380  // Timer Initial Count Register
#define LAPIC_TIMER_CCR         0x390  // Timer Current Count Register
#define LAPIC_TIMER_DCR         0x3E0  // Timer Divide Configuration Register

// LAPIC Spurious Vector Register flags
#define LAPIC_SPURIOUS_ENABLE   (1 << 8)   // APIC Software Enable
#define LAPIC_SPURIOUS_VECTOR   0xFF        // Spurious vector mask

// LAPIC LVT flags
#define LAPIC_LVT_MASKED        (1 << 16)   // Interrupt masked
#define LAPIC_LVT_LEVEL         (1 << 15)   // Level triggered
#define LAPIC_LVT_ACTIVE_LOW    (1 << 13)   // Active low
#define LAPIC_LVT_PENDING       (1 << 12)   // Delivery pending

// Timer modes
#define LAPIC_TIMER_ONESHOT     0x00000000
#define LAPIC_TIMER_PERIODIC    0x00020000
#define LAPIC_TIMER_TSC_DEADLINE 0x00040000

// Timer divisor values
#define LAPIC_TIMER_DIV_2       0x0
#define LAPIC_TIMER_DIV_4       0x1
#define LAPIC_TIMER_DIV_8       0x2
#define LAPIC_TIMER_DIV_16      0x3
#define LAPIC_TIMER_DIV_32      0x8
#define LAPIC_TIMER_DIV_64      0x9
#define LAPIC_TIMER_DIV_128     0xA
#define LAPIC_TIMER_DIV_1       0xB

// APIC detection and initialization
int apic_detect(void);
int apic_init(void);
int apic_is_available(void);
int apic_is_x2apic_available(void);
int apic_is_bsp(void);
int apic_is_enabled(void);

// APIC control
void apic_enable(void);
void apic_disable(void);
void apic_send_eoi(void);
uint32_t apic_get_id(void);
uint32_t apic_get_version(void);

// APIC timer
void apic_timer_init(uint32_t vector, uint32_t frequency);
void apic_timer_start(uint32_t initial_count);
void apic_timer_stop(void);
uint32_t apic_timer_get_current_count(void);
void apic_timer_set_divisor(uint32_t divisor);

// Utility functions
void apic_dump_state(void);
uint64_t apic_get_base_address(void);
void apic_set_base_address(uint64_t base);

// Low-level register access
uint32_t apic_read_register(uint32_t reg);
void apic_write_register(uint32_t reg, uint32_t value);

// MSR access utilities
uint64_t read_msr(uint32_t msr);
void write_msr(uint32_t msr, uint64_t value);

// CPUID utilities
void cpuid(uint32_t leaf, uint32_t *eax, uint32_t *ebx, uint32_t *ecx, uint32_t *edx);

#endif // APIC_H
