/*
 * SlopOS APIC (Advanced Programmable Interrupt Controller) Driver
 * Local APIC and I/O APIC detection and basic initialization
 */

#include "apic.h"
#include "serial.h"
#include "../lib/klog.h"

// Limine boot protocol exports
extern uint64_t get_hhdm_offset(void);
extern int is_hhdm_available(void);

// Global APIC state
static int apic_available = 0;
static int x2apic_available = 0;
static uint64_t apic_base_address = 0;
static uint64_t apic_base_physical = 0;
static int apic_enabled = 0;

/*
 * Read MSR (Model Specific Register)
 */
uint64_t read_msr(uint32_t msr) {
    uint32_t low, high;
    __asm__ volatile ("rdmsr" : "=a" (low), "=d" (high) : "c" (msr));
    return ((uint64_t)high << 32) | low;
}

/*
 * Write MSR (Model Specific Register)
 */
void write_msr(uint32_t msr, uint64_t value) {
    uint32_t low = value & 0xFFFFFFFF;
    uint32_t high = value >> 32;
    __asm__ volatile ("wrmsr" : : "a" (low), "d" (high), "c" (msr));
}

/*
 * Execute CPUID instruction
 */
void cpuid(uint32_t leaf, uint32_t *eax, uint32_t *ebx, uint32_t *ecx, uint32_t *edx) {
    __asm__ volatile ("cpuid"
                      : "=a" (*eax), "=b" (*ebx), "=c" (*ecx), "=d" (*edx)
                      : "a" (leaf));
}

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

        KLOG_BLOCK(KLOG_DEBUG, {
            klog_raw(KLOG_INFO, "APIC: Physical base: ");
            klog_hex(KLOG_INFO, apic_base_physical);
            klog(KLOG_INFO, "");
        });

        if (is_hhdm_available()) {
            uint64_t hhdm_offset = get_hhdm_offset();
            apic_base_address = apic_base_physical + hhdm_offset;

            KLOG_BLOCK(KLOG_DEBUG, {
                klog_raw(KLOG_INFO, "APIC: Virtual base (HHDM): ");
                klog_hex(KLOG_INFO, apic_base_address);
                klog(KLOG_INFO, "");
            });
        } else {
            klog_info("APIC: ERROR - HHDM not available, cannot map APIC registers");
            apic_available = 0;
            return 0;
        }

        KLOG_BLOCK(KLOG_DEBUG, {
            klog_raw(KLOG_INFO, "APIC: MSR flags: ");
            if (apic_base_msr & APIC_BASE_BSP) klog_raw(KLOG_INFO, "BSP ");
            if (apic_base_msr & APIC_BASE_X2APIC) klog_raw(KLOG_INFO, "X2APIC ");
            if (apic_base_msr & APIC_BASE_GLOBAL_ENABLE) klog_raw(KLOG_INFO, "ENABLED ");
            klog(KLOG_INFO, "");
        });

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
        klog(KLOG_INFO, "APIC: Cannot initialize - APIC not available");
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

    KLOG_BLOCK(KLOG_DEBUG, {
        klog_raw(KLOG_INFO, "APIC: ID: ");
        klog_hex(KLOG_INFO, apic_id);
        klog_raw(KLOG_INFO, ", Version: ");
        klog_hex(KLOG_INFO, apic_version);
        klog(KLOG_INFO, "");
    });

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

    KLOG_BLOCK(KLOG_DEBUG, {
        klog_raw(KLOG_INFO, "APIC: Initializing timer with vector ");
        klog_hex(KLOG_INFO, vector);
        klog_raw(KLOG_INFO, " and frequency ");
        klog_decimal(KLOG_INFO, frequency);
        klog(KLOG_INFO, "");
    });

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
    klog(KLOG_INFO, "=== APIC STATE DUMP ===");

    if (!apic_available) {
        klog(KLOG_INFO, "APIC: Not available");
        klog(KLOG_INFO, "=== END APIC STATE DUMP ===");
        return;
    }

    klog_raw(KLOG_INFO, "APIC Available: Yes, x2APIC: ");
    klog(KLOG_INFO, x2apic_available ? "Yes" : "No");

    klog_raw(KLOG_INFO, "APIC Enabled: ");
    klog(KLOG_INFO, apic_enabled ? "Yes" : "No");

    klog_raw(KLOG_INFO, "Bootstrap Processor: ");
    klog(KLOG_INFO, apic_is_bsp() ? "Yes" : "No");

    klog_raw(KLOG_INFO, "Base Address: ");
    klog_hex(KLOG_INFO, apic_base_address);
    klog(KLOG_INFO, "");

    if (apic_enabled) {
        klog_raw(KLOG_INFO, "APIC ID: ");
        klog_hex(KLOG_INFO, apic_get_id());
        klog(KLOG_INFO, "");

        klog_raw(KLOG_INFO, "APIC Version: ");
        klog_hex(KLOG_INFO, apic_get_version());
        klog(KLOG_INFO, "");

        uint32_t spurious = apic_read_register(LAPIC_SPURIOUS);
        klog_raw(KLOG_INFO, "Spurious Vector Register: ");
        klog_hex(KLOG_INFO, spurious);
        klog(KLOG_INFO, "");

        uint32_t esr = apic_read_register(LAPIC_ESR);
        klog_raw(KLOG_INFO, "Error Status Register: ");
        klog_hex(KLOG_INFO, esr);
        klog(KLOG_INFO, "");

        uint32_t lvt_timer = apic_read_register(LAPIC_LVT_TIMER);
        klog_raw(KLOG_INFO, "Timer LVT: ");
        klog_hex(KLOG_INFO, lvt_timer);
        if (lvt_timer & LAPIC_LVT_MASKED) klog_raw(KLOG_INFO, " (MASKED)");
        klog(KLOG_INFO, "");

        uint32_t timer_count = apic_timer_get_current_count();
        klog_raw(KLOG_INFO, "Timer Current Count: ");
        klog_hex(KLOG_INFO, timer_count);
        klog(KLOG_INFO, "");
    }

    klog(KLOG_INFO, "=== END APIC STATE DUMP ===");
}
