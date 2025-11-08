/*
 * SlopOS APIC (Advanced Programmable Interrupt Controller) Driver
 * Local APIC and I/O APIC detection and basic initialization
 */

#include "apic.h"
#include "serial.h"
#include "../boot/log.h"

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

    boot_log_debug("APIC: Detecting Local APIC availability");

    // Check CPUID leaf 1 for APIC support
    cpuid(1, &eax, &ebx, &ecx, &edx);

    // Check for Local APIC in EDX bit 9
    if (edx & CPUID_FEAT_EDX_APIC) {
        apic_available = 1;
        boot_log_debug("APIC: Local APIC is available");

        // Check for x2APIC in ECX bit 21
        if (ecx & CPUID_FEAT_ECX_X2APIC) {
            x2apic_available = 1;
            boot_log_debug("APIC: x2APIC mode is available");
        } else {
            boot_log_debug("APIC: x2APIC mode is not available");
        }

        // Get APIC base address from MSR
        uint64_t apic_base_msr = read_msr(MSR_APIC_BASE);
        apic_base_physical = apic_base_msr & APIC_BASE_ADDR_MASK;

        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
            kprint("APIC: Physical base: ");
            kprint_hex(apic_base_physical);
            kprintln("");
        });

        if (is_hhdm_available()) {
            uint64_t hhdm_offset = get_hhdm_offset();
            apic_base_address = apic_base_physical + hhdm_offset;

            BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
                kprint("APIC: Virtual base (HHDM): ");
                kprint_hex(apic_base_address);
                kprintln("");
            });
        } else {
            boot_log_info("APIC: ERROR - HHDM not available, cannot map APIC registers");
            apic_available = 0;
            return 0;
        }

        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
            kprint("APIC: MSR flags: ");
            if (apic_base_msr & APIC_BASE_BSP) kprint("BSP ");
            if (apic_base_msr & APIC_BASE_X2APIC) kprint("X2APIC ");
            if (apic_base_msr & APIC_BASE_GLOBAL_ENABLE) kprint("ENABLED ");
            kprintln("");
        });

        return 1;
    } else {
        boot_log_debug("APIC: Local APIC is not available");
        return 0;
    }
}

/*
 * Initialize APIC
 */
int apic_init(void) {
    if (!apic_available) {
        kprintln("APIC: Cannot initialize - APIC not available");
        return -1;
    }

    boot_log_debug("APIC: Initializing Local APIC");

    // Enable APIC globally in MSR if not already enabled
    uint64_t apic_base_msr = read_msr(MSR_APIC_BASE);
    if (!(apic_base_msr & APIC_BASE_GLOBAL_ENABLE)) {
        apic_base_msr |= APIC_BASE_GLOBAL_ENABLE;
        write_msr(MSR_APIC_BASE, apic_base_msr);
        boot_log_debug("APIC: Enabled APIC globally via MSR");
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

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("APIC: ID: ");
        kprint_hex(apic_id);
        kprint(", Version: ");
        kprint_hex(apic_version);
        kprintln("");
    });

    apic_enabled = 1;
    boot_log_debug("APIC: Initialization complete");

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
    boot_log_debug("APIC: Local APIC enabled");
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
    boot_log_debug("APIC: Local APIC disabled");
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

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("APIC: Initializing timer with vector ");
        kprint_hex(vector);
        kprint(" and frequency ");
        kprint_dec(frequency);
        kprintln("");
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

    boot_log_debug("APIC: Timer initialized");
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
    kprintln("=== APIC STATE DUMP ===");

    if (!apic_available) {
        kprintln("APIC: Not available");
        kprintln("=== END APIC STATE DUMP ===");
        return;
    }

    kprint("APIC Available: Yes, x2APIC: ");
    kprintln(x2apic_available ? "Yes" : "No");

    kprint("APIC Enabled: ");
    kprintln(apic_enabled ? "Yes" : "No");

    kprint("Bootstrap Processor: ");
    kprintln(apic_is_bsp() ? "Yes" : "No");

    kprint("Base Address: ");
    kprint_hex(apic_base_address);
    kprintln("");

    if (apic_enabled) {
        kprint("APIC ID: ");
        kprint_hex(apic_get_id());
        kprintln("");

        kprint("APIC Version: ");
        kprint_hex(apic_get_version());
        kprintln("");

        uint32_t spurious = apic_read_register(LAPIC_SPURIOUS);
        kprint("Spurious Vector Register: ");
        kprint_hex(spurious);
        kprintln("");

        uint32_t esr = apic_read_register(LAPIC_ESR);
        kprint("Error Status Register: ");
        kprint_hex(esr);
        kprintln("");

        uint32_t lvt_timer = apic_read_register(LAPIC_LVT_TIMER);
        kprint("Timer LVT: ");
        kprint_hex(lvt_timer);
        if (lvt_timer & LAPIC_LVT_MASKED) kprint(" (MASKED)");
        kprintln("");

        uint32_t timer_count = apic_timer_get_current_count();
        kprint("Timer Current Count: ");
        kprint_hex(timer_count);
        kprintln("");
    }

    kprintln("=== END APIC STATE DUMP ===");
}
