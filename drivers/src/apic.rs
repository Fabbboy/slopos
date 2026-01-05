use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_lib::{cpu, klog_debug, klog_info};

use crate::hw::apic_defs::*;
use crate::sched_bridge;
use crate::wl_currency;

static APIC_AVAILABLE: AtomicBool = AtomicBool::new(false);
static X2APIC_AVAILABLE: AtomicBool = AtomicBool::new(false);
static APIC_ENABLED: AtomicBool = AtomicBool::new(false);
static APIC_BASE_ADDRESS: AtomicU64 = AtomicU64::new(0);
static APIC_BASE_PHYSICAL: AtomicU64 = AtomicU64::new(0);

#[inline]
fn hhdm_virt_for(phys: u64) -> Option<u64> {
    if phys == 0 {
        return None;
    }
    if sched_bridge::is_hhdm_available() != 0 {
        Some(phys + sched_bridge::get_hhdm_offset())
    } else {
        None
    }
}

pub fn detect() -> bool {
    klog_debug!("APIC: Detecting Local APIC availability...");

    let (_, _, ecx, edx) = cpu::cpuid(1);
    if edx & CPUID_FEAT_EDX_APIC == 0 {
        klog_debug!("APIC: Local APIC is not available");
        APIC_AVAILABLE.store(false, Ordering::Relaxed);
        return false;
    }

    APIC_AVAILABLE.store(true, Ordering::Relaxed);
    let x2 = (ecx & CPUID_FEAT_ECX_X2APIC) != 0;
    X2APIC_AVAILABLE.store(x2, Ordering::Relaxed);

    let apic_base_msr = cpu::read_msr(MSR_APIC_BASE);
    let apic_phys = apic_base_msr & APIC_BASE_ADDR_MASK;
    APIC_BASE_PHYSICAL.store(apic_phys, Ordering::Relaxed);

    if let Some(virt) = hhdm_virt_for(apic_phys) {
        APIC_BASE_ADDRESS.store(virt, Ordering::Relaxed);
        let bsp_flag = if apic_base_msr & APIC_BASE_BSP != 0 {
            " BSP"
        } else {
            ""
        };
        let x2apic_flag = if apic_base_msr & APIC_BASE_X2APIC != 0 {
            " X2APIC"
        } else {
            ""
        };
        let enable_flag = if apic_base_msr & APIC_BASE_GLOBAL_ENABLE != 0 {
            " ENABLED"
        } else {
            ""
        };
        klog_debug!(
            "APIC: Physical base: 0x{:x}, Virtual base (HHDM): 0x{:x}",
            apic_phys,
            virt
        );
        klog_debug!("APIC: MSR flags:{}{}{}", bsp_flag, x2apic_flag, enable_flag);
        true
    } else {
        klog_info!("APIC: ERROR - HHDM not available, cannot map APIC registers");
        APIC_AVAILABLE.store(false, Ordering::Relaxed);
        false
    }
}

pub fn init() -> i32 {
    if !is_available() {
        klog_info!("APIC: Cannot initialize - APIC not available");
        wl_currency::award_loss();
        return -1;
    }

    klog_debug!("APIC: Initializing Local APIC");

    let mut apic_base_msr = cpu::read_msr(MSR_APIC_BASE);
    if apic_base_msr & APIC_BASE_GLOBAL_ENABLE == 0 {
        apic_base_msr |= APIC_BASE_GLOBAL_ENABLE;
        cpu::write_msr(MSR_APIC_BASE, apic_base_msr);
        klog_debug!("APIC: Enabled APIC globally via MSR");
    }

    enable();

    // Mask all LVT entries to prevent spurious interrupts.
    write_register(LAPIC_LVT_TIMER, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_LINT0, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_LINT1, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_ERROR, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_PERFCNT, LAPIC_LVT_MASKED);

    // Route legacy PIC interrupts through LINT0 in ExtINT mode.
    write_register(LAPIC_LVT_LINT0, LAPIC_LVT_DELIVERY_MODE_EXTINT);

    // Clear error status register twice per Intel SDM guidance.
    write_register(LAPIC_ESR, 0);
    write_register(LAPIC_ESR, 0);

    send_eoi();

    let apic_id = get_id();
    let apic_version = get_version();
    klog_debug!("APIC: ID: 0x{:x}, Version: 0x{:x}", apic_id, apic_version);

    APIC_ENABLED.store(true, Ordering::Relaxed);
    klog_debug!("APIC: Initialization complete");
    wl_currency::award_win();
    0
}

pub fn is_available() -> bool {
    APIC_AVAILABLE.load(Ordering::Relaxed)
}

pub fn is_x2apic_available() -> bool {
    X2APIC_AVAILABLE.load(Ordering::Relaxed)
}

pub fn is_bsp() -> bool {
    if !is_available() {
        return false;
    }
    let apic_base_msr = cpu::read_msr(MSR_APIC_BASE);
    (apic_base_msr & APIC_BASE_BSP) != 0
}

pub fn is_enabled() -> bool {
    APIC_ENABLED.load(Ordering::Relaxed)
}

pub fn enable() {
    if !is_available() {
        return;
    }
    let mut spurious = read_register(LAPIC_SPURIOUS);
    spurious |= LAPIC_SPURIOUS_ENABLE;
    spurious |= 0xFF;
    write_register(LAPIC_SPURIOUS, spurious);
    APIC_ENABLED.store(true, Ordering::Relaxed);
    klog_debug!("APIC: Local APIC enabled");
}

pub fn disable() {
    if !is_available() {
        return;
    }
    let mut spurious = read_register(LAPIC_SPURIOUS);
    spurious &= !LAPIC_SPURIOUS_ENABLE;
    write_register(LAPIC_SPURIOUS, spurious);
    APIC_ENABLED.store(false, Ordering::Relaxed);
    klog_debug!("APIC: Local APIC disabled");
}

pub fn send_eoi() {
    if !is_enabled() {
        return;
    }
    write_register(LAPIC_EOI, 0);
}

pub fn get_id() -> u32 {
    if !is_available() {
        return 0;
    }
    read_register(LAPIC_ID) >> 24
}

pub fn get_version() -> u32 {
    if !is_available() {
        return 0;
    }
    read_register(LAPIC_VERSION) & 0xFF
}

pub fn timer_init(vector: u32, frequency: u32) {
    if !is_enabled() {
        return;
    }
    klog_debug!(
        "APIC: Initializing timer with vector 0x{:x} and frequency {}",
        vector,
        frequency
    );

    timer_set_divisor(LAPIC_TIMER_DIV_16);

    let lvt_timer = vector | LAPIC_TIMER_PERIODIC;
    write_register(LAPIC_LVT_TIMER, lvt_timer);

    let initial_count = 1_000_000u32.saturating_div(frequency.max(1));
    timer_start(initial_count);
    klog_debug!("APIC: Timer initialized");
}

pub fn timer_start(initial_count: u32) {
    if !is_enabled() {
        return;
    }
    write_register(LAPIC_TIMER_ICR, initial_count);
}

pub fn timer_stop() {
    if !is_enabled() {
        return;
    }
    write_register(LAPIC_TIMER_ICR, 0);
}

pub fn timer_get_current_count() -> u32 {
    if !is_enabled() {
        return 0;
    }
    read_register(LAPIC_TIMER_CCR)
}

pub fn timer_set_divisor(divisor: u32) {
    if !is_enabled() {
        return;
    }
    write_register(LAPIC_TIMER_DCR, divisor);
}

pub fn send_ipi_halt_all() {
    if !is_available() || !is_enabled() {
        return;
    }

    // Vector 0xFE is commonly used for shutdown IPI
    const SHUTDOWN_VECTOR: u32 = 0xFE;

    // Wait for any pending IPI to complete (delivery status bit must be clear)
    let mut timeout = 1000;
    while (read_register(LAPIC_ICR_LOW) & LAPIC_ICR_DELIVERY_STATUS) != 0 && timeout > 0 {
        cpu::pause();
        timeout -= 1;
    }

    // Write destination to ICR_HIGH (broadcast to all except self in physical mode)
    write_register(LAPIC_ICR_HIGH, LAPIC_ICR_DEST_BROADCAST);

    // Write IPI command to ICR_LOW:
    // - Vector: 0xFE (shutdown)
    // - Delivery mode: Fixed (0)
    // - Destination mode: Physical (0)
    // - Level: Asserted (1)
    // - Trigger: Edge (0)
    let icr_low = SHUTDOWN_VECTOR
        | LAPIC_ICR_DELIVERY_FIXED
        | LAPIC_ICR_DEST_PHYSICAL
        | LAPIC_ICR_LEVEL_ASSERT
        | LAPIC_ICR_TRIGGER_EDGE;
    write_register(LAPIC_ICR_LOW, icr_low);

    // Wait for IPI to be sent (delivery status bit must clear)
    timeout = 1000;
    while (read_register(LAPIC_ICR_LOW) & LAPIC_ICR_DELIVERY_STATUS) != 0 && timeout > 0 {
        cpu::pause();
        timeout -= 1;
    }

    klog_debug!("APIC: Sent shutdown IPI to all processors");
}

pub fn get_base_address() -> u64 {
    APIC_BASE_ADDRESS.load(Ordering::Relaxed)
}

pub fn set_base_address(base: u64) {
    if !is_available() {
        return;
    }
    let masked_base = base & APIC_BASE_ADDR_MASK;
    let mut apic_base_msr = cpu::read_msr(MSR_APIC_BASE);
    apic_base_msr = (apic_base_msr & !APIC_BASE_ADDR_MASK) | masked_base;
    cpu::write_msr(MSR_APIC_BASE, apic_base_msr);

    APIC_BASE_PHYSICAL.store(masked_base, Ordering::Relaxed);
    if let Some(virt) = hhdm_virt_for(masked_base) {
        APIC_BASE_ADDRESS.store(virt, Ordering::Relaxed);
    } else {
        APIC_BASE_ADDRESS.store(0, Ordering::Relaxed);
    }
}

pub fn read_register(reg: u32) -> u32 {
    let base = APIC_BASE_ADDRESS.load(Ordering::Relaxed);
    if !is_available() || base == 0 {
        return 0;
    }
    let reg_ptr = (base + reg as u64) as *const u32;
    unsafe { read_volatile(reg_ptr) }
}

pub fn write_register(reg: u32, value: u32) {
    let base = APIC_BASE_ADDRESS.load(Ordering::Relaxed);
    if !is_available() || base == 0 {
        return;
    }
    let reg_ptr = (base + reg as u64) as *mut u32;
    unsafe { write_volatile(reg_ptr, value) };
}

pub fn dump_state() {
    klog_info!("=== APIC STATE DUMP ===");
    if !is_available() {
        klog_info!("APIC: Not available");
        klog_info!("=== END APIC STATE DUMP ===");
        return;
    }

    klog_info!(
        "APIC Available: Yes, x2APIC: {}",
        if is_x2apic_available() { "Yes" } else { "No" }
    );
    klog_info!("APIC Enabled: {}", if is_enabled() { "Yes" } else { "No" });
    klog_info!(
        "Bootstrap Processor: {}",
        if is_bsp() { "Yes" } else { "No" }
    );
    klog_info!("Base Address: 0x{:x}", get_base_address());

    if is_enabled() {
        let spurious = read_register(LAPIC_SPURIOUS);
        let esr = read_register(LAPIC_ESR);
        let lvt_timer = read_register(LAPIC_LVT_TIMER);
        let timer_count = timer_get_current_count();
        klog_info!("APIC ID: 0x{:x}", get_id());
        klog_info!("APIC Version: 0x{:x}", get_version());
        klog_info!("Spurious Vector Register: 0x{:x}", spurious);
        klog_info!("Error Status Register: 0x{:x}", esr);
        klog_info!(
            "Timer LVT: 0x{:x}{}",
            lvt_timer,
            if lvt_timer & LAPIC_LVT_MASKED != 0 {
                " (MASKED)"
            } else {
                ""
            }
        );
        klog_info!("Timer Current Count: 0x{:x}", timer_count);
    }

    klog_info!("=== END APIC STATE DUMP ===");
}
pub fn apic_detect() -> i32 {
    if detect() { 1 } else { 0 }
}
pub fn apic_init() -> i32 {
    init()
}
pub fn apic_is_available() -> i32 {
    if is_available() { 1 } else { 0 }
}
pub fn apic_is_x2apic_available() -> i32 {
    if is_x2apic_available() { 1 } else { 0 }
}
pub fn apic_is_bsp() -> i32 {
    if is_bsp() { 1 } else { 0 }
}
pub fn apic_is_enabled() -> i32 {
    if is_enabled() { 1 } else { 0 }
}
pub fn apic_enable() {
    enable();
}
pub fn apic_disable() {
    disable();
}
pub fn apic_send_eoi() {
    send_eoi();
}
pub fn apic_get_id() -> u32 {
    get_id()
}
pub fn apic_get_version() -> u32 {
    get_version()
}
pub fn apic_timer_init(vector: u32, frequency: u32) {
    timer_init(vector, frequency);
}
pub fn apic_timer_start(initial_count: u32) {
    timer_start(initial_count);
}
pub fn apic_timer_stop() {
    timer_stop();
}
pub fn apic_timer_get_current_count() -> u32 {
    timer_get_current_count()
}
pub fn apic_timer_set_divisor(divisor: u32) {
    timer_set_divisor(divisor);
}
pub fn apic_dump_state() {
    dump_state();
}
pub fn apic_get_base_address() -> u64 {
    get_base_address()
}
pub fn apic_set_base_address(base: u64) {
    set_base_address(base);
}
pub fn apic_read_register(reg: u32) -> u32 {
    read_register(reg)
}
pub fn apic_write_register(reg: u32, value: u32) {
    write_register(reg, value);
}
pub fn apic_send_ipi_halt_all() {
    send_ipi_halt_all();
}
