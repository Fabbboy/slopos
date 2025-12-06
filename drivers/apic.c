/*
 * SlopOS APIC (Advanced Programmable Interrupt Controller) Driver
 * Local APIC and I/O APIC detection and basic initialization
 */

#include "apic.h"
#include "serial.h"
#include "../lib/klog.h"
#include "../lib/cpu.h"
#include "../boot/limine_protocol.h"

// Global APIC state
static int apic_available = 0;
static int x2apic_available = 0;
static uint64_t apic_base_address = 0;
static uint64_t apic_base_physical = 0;
static int apic_enabled = 0;

/*
 * Detect APIC availability
 */
int apic_detect(void) {
    uint32_t eax, ebx, ecx, edx;

    klog_debug("APIC: Detecting Local APIC availability");

    // Check CPUID leaf 1 for APIC support
    cpuid(1, &eax, &ebx, &ecx, &edx);

    // Check for Local APIC in EDX bit 9
    if (edx & CPUID_FEAT_EDX_APIC) {
        apic_available = 1;
        klog_debug("APIC: Local APIC is available");

        // Check for x2APIC in ECX bit 21
        if (ecx & CPUID_FEAT_ECX_X2APIC) {
            x2apic_available = 1;
            klog_debug("APIC: x2APIC mode is available");
        } else {
            klog_debug("APIC: x2APIC mode is not available");
        }

        // Get APIC base address from MSR
        uint64_t apic_base_msr = read_msr(MSR_APIC_BASE);
        apic_base_physical = apic_base_msr & APIC_BASE_ADDR_MASK;

        klog_printf(KLOG_DEBUG, "APIC: Physical base: 0x%llx\n",
                    (unsigned long long)apic_base_physical);

        if (is_hhdm_available()) {
            uint64_t hhdm_offset = get_hhdm_offset();
            apic_base_address = apic_base_physical + hhdm_offset;

            klog_printf(KLOG_DEBUG, "APIC: Virtual base (HHDM): 0x%llx\n",
                        (unsigned long long)apic_base_address);
        } else {
            klog_info("APIC: ERROR - HHDM not available, cannot map APIC registers");
            apic_available = 0;
            return 0;
        }

        klog_printf(KLOG_DEBUG, "APIC: MSR flags:%s%s%s\n",
                    (apic_base_msr & APIC_BASE_BSP) ? " BSP" : "",
                    (apic_base_msr & APIC_BASE_X2APIC) ? " X2APIC" : "",
                    (apic_base_msr & APIC_BASE_GLOBAL_ENABLE) ? " ENABLED" : "");

        return 1;
    } else {
        klog_debug("APIC: Local APIC is not available");
        return 0;
    }
}

/*
 * Initialize APIC
 */
int apic_init(void) {
    if (!apic_available) {
        klog_printf(KLOG_INFO, "APIC: Cannot initialize - APIC not available\n");
        return -1;
    }

    klog_debug("APIC: Initializing Local APIC");

    // Enable APIC globally in MSR if not already enabled
    uint64_t apic_base_msr = read_msr(MSR_APIC_BASE);
    if (!(apic_base_msr & APIC_BASE_GLOBAL_ENABLE)) {
        apic_base_msr |= APIC_BASE_GLOBAL_ENABLE;
        write_msr(MSR_APIC_BASE, apic_base_msr);
        klog_debug("APIC: Enabled APIC globally via MSR");
    }

    // Enable APIC via spurious vector register
    apic_enable();

    // Mask all LVT entries to prevent spurious interrupts
    apic_write_register(LAPIC_LVT_TIMER, LAPIC_LVT_MASKED);
    apic_write_register(LAPIC_LVT_LINT0, LAPIC_LVT_MASKED);
    apic_write_register(LAPIC_LVT_LINT1, LAPIC_LVT_MASKED);
    apic_write_register(LAPIC_LVT_ERROR, LAPIC_LVT_MASKED);
    apic_write_register(LAPIC_LVT_PERFCNT, LAPIC_LVT_MASKED);

    /* Route legacy PIC interrupts through LINT0 in ExtINT mode */
    uint32_t lint0 = LAPIC_LVT_DELIVERY_MODE_EXTINT;
    apic_write_register(LAPIC_LVT_LINT0, lint0);

    // Clear error status register
    apic_write_register(LAPIC_ESR, 0);
    apic_write_register(LAPIC_ESR, 0);  // Write twice as per Intel manual

    // Clear any pending EOI
    apic_send_eoi();

    uint32_t apic_id = apic_get_id();
    uint32_t apic_version = apic_get_version();

    klog_printf(KLOG_DEBUG, "APIC: ID: 0x%x, Version: 0x%x\n", apic_id, apic_version);

    apic_enabled = 1;
    klog_debug("APIC: Initialization complete");

    return 0;
}

/*
 * Check if APIC is available
 */
int apic_is_available(void) {
    return apic_available;
}

/*
 * Check if x2APIC is available
 */
int apic_is_x2apic_available(void) {
    return x2apic_available;
}

/*
 * Check if this is the Bootstrap Processor
 */
int apic_is_bsp(void) {
    if (!apic_available) return 0;
    uint64_t apic_base_msr = read_msr(MSR_APIC_BASE);
    return (apic_base_msr & APIC_BASE_BSP) != 0;
}

int apic_is_enabled(void) {
    return apic_enabled;
}

/*
 * Enable APIC
 */
void apic_enable(void) {
    if (!apic_available) return;

    // Enable APIC via spurious vector register
    uint32_t spurious = apic_read_register(LAPIC_SPURIOUS);
    spurious |= LAPIC_SPURIOUS_ENABLE;
    spurious |= 0xFF;  // Set spurious vector to 255
    apic_write_register(LAPIC_SPURIOUS, spurious);

    apic_enabled = 1;
    klog_debug("APIC: Local APIC enabled");
}

/*
 * Disable APIC
 */
void apic_disable(void) {
    if (!apic_available) return;

    // Disable APIC via spurious vector register
    uint32_t spurious = apic_read_register(LAPIC_SPURIOUS);
    spurious &= ~LAPIC_SPURIOUS_ENABLE;
    apic_write_register(LAPIC_SPURIOUS, spurious);

    apic_enabled = 0;
    klog_debug("APIC: Local APIC disabled");
}

/*
 * Send End of Interrupt
 */
void apic_send_eoi(void) {
    if (!apic_enabled) return;
    apic_write_register(LAPIC_EOI, 0);
}

/*
 * Get APIC ID
 */
uint32_t apic_get_id(void) {
    if (!apic_available) return 0;
    uint32_t id = apic_read_register(LAPIC_ID);
    return id >> 24;  // APIC ID is in bits 31:24
}

/*
 * Get APIC version
 */
uint32_t apic_get_version(void) {
    if (!apic_available) return 0;
    return apic_read_register(LAPIC_VERSION) & 0xFF;
}

/*
 * Initialize APIC timer
 */
void apic_timer_init(uint32_t vector, uint32_t frequency) {
    if (!apic_enabled) return;

    klog_printf(KLOG_DEBUG, "APIC: Initializing timer with vector 0x%x and frequency %u\n",
                vector, frequency);

    // Set timer divisor to 16
    apic_timer_set_divisor(LAPIC_TIMER_DIV_16);

    // Configure timer LVT
    uint32_t lvt_timer = vector | LAPIC_TIMER_PERIODIC;
    apic_write_register(LAPIC_LVT_TIMER, lvt_timer);

    // Calculate initial count for desired frequency
    // This is a rough calculation - would need calibration for accuracy
    uint32_t initial_count = 1000000 / frequency;  // Approximate
    apic_timer_start(initial_count);

    klog_debug("APIC: Timer initialized");
}

/*
 * Start APIC timer
 */
void apic_timer_start(uint32_t initial_count) {
    if (!apic_enabled) return;
    apic_write_register(LAPIC_TIMER_ICR, initial_count);
}

/*
 * Stop APIC timer
 */
void apic_timer_stop(void) {
    if (!apic_enabled) return;
    apic_write_register(LAPIC_TIMER_ICR, 0);
}

/*
 * Get current timer count
 */
uint32_t apic_timer_get_current_count(void) {
    if (!apic_enabled) return 0;
    return apic_read_register(LAPIC_TIMER_CCR);
}

/*
 * Set timer divisor
 */
void apic_timer_set_divisor(uint32_t divisor) {
    if (!apic_enabled) return;
    apic_write_register(LAPIC_TIMER_DCR, divisor);
}

/*
 * Get APIC base address
 */
uint64_t apic_get_base_address(void) {
    return apic_base_address;
}

/*
 * Set APIC base address
 */
void apic_set_base_address(uint64_t base) {
    if (!apic_available) return;

    uint64_t masked_base = base & APIC_BASE_ADDR_MASK;
    uint64_t apic_base_msr = read_msr(MSR_APIC_BASE);
    apic_base_msr = (apic_base_msr & ~APIC_BASE_ADDR_MASK) | masked_base;
    write_msr(MSR_APIC_BASE, apic_base_msr);

    apic_base_physical = masked_base;
    if (is_hhdm_available()) {
        apic_base_address = apic_base_physical + get_hhdm_offset();
    } else {
        apic_base_address = 0;
    }
}

/*
 * Read APIC register
 */
uint32_t apic_read_register(uint32_t reg) {
    if (!apic_available || apic_base_address == 0) return 0;

    // Memory-mapped access to APIC registers
    volatile uint32_t *apic_reg = (volatile uint32_t *)(uintptr_t)(apic_base_address + reg);
    return *apic_reg;
}

/*
 * Write APIC register
 */
void apic_write_register(uint32_t reg, uint32_t value) {
    if (!apic_available || apic_base_address == 0) return;

    // Memory-mapped access to APIC registers
    volatile uint32_t *apic_reg = (volatile uint32_t *)(uintptr_t)(apic_base_address + reg);
    *apic_reg = value;
}

/*
 * Dump APIC state for debugging
 */
void apic_dump_state(void) {
    klog_printf(KLOG_INFO, "=== APIC STATE DUMP ===\n");

    if (!apic_available) {
        klog_printf(KLOG_INFO, "APIC: Not available\n");
        klog_printf(KLOG_INFO, "=== END APIC STATE DUMP ===\n");
        return;
    }

    klog_printf(KLOG_INFO, "APIC Available: Yes, x2APIC: %s\n", x2apic_available ? "Yes" : "No");
    klog_printf(KLOG_INFO, "APIC Enabled: %s\n", apic_enabled ? "Yes" : "No");
    klog_printf(KLOG_INFO, "Bootstrap Processor: %s\n", apic_is_bsp() ? "Yes" : "No");
    klog_printf(KLOG_INFO, "Base Address: 0x%llx\n", (unsigned long long)apic_base_address);

    if (apic_enabled) {
        klog_printf(KLOG_INFO, "APIC ID: 0x%x\n", apic_get_id());
        klog_printf(KLOG_INFO, "APIC Version: 0x%x\n", apic_get_version());

        uint32_t spurious = apic_read_register(LAPIC_SPURIOUS);
        klog_printf(KLOG_INFO, "Spurious Vector Register: 0x%x\n", spurious);

        uint32_t esr = apic_read_register(LAPIC_ESR);
        klog_printf(KLOG_INFO, "Error Status Register: 0x%x\n", esr);

        uint32_t lvt_timer = apic_read_register(LAPIC_LVT_TIMER);
        klog_printf(KLOG_INFO, "Timer LVT: 0x%x%s\n",
                    lvt_timer,
                    (lvt_timer & LAPIC_LVT_MASKED) ? " (MASKED)" : "");

        uint32_t timer_count = apic_timer_get_current_count();
        klog_printf(KLOG_INFO, "Timer Current Count: 0x%x\n", timer_count);
    }

    klog_printf(KLOG_INFO, "=== END APIC STATE DUMP ===\n");
}
