pub(crate) mod regs;
pub mod timer;

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_arch::cpu;
use slopos_ostd::sync::InitFlag;
use slopos_ostd::sync::OnceLock;
use slopos_ostd::{klog_debug, klog_info};

use regs::*;
use slopos_abi::addr::PhysAddr;
use slopos_arch::cpu::apic_msr::ApicBaseMsr;
use slopos_arch::cpu::cpuid::{CPUID_FEAT_ECX_X2APIC, CPUID_FEAT_EDX_APIC, CPUID_LEAF_FEATURES};
use slopos_arch::cpu::msr::Msr;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_mm::paging_defs::PAGE_SIZE_4KB_USIZE;

const APIC_REGION_SIZE: usize = PAGE_SIZE_4KB_USIZE;

static APIC_AVAILABLE: InitFlag = InitFlag::new();
static X2APIC_AVAILABLE: InitFlag = InitFlag::new();
static APIC_ENABLED: InitFlag = InitFlag::new();
static APIC_BASE_PHYSICAL: AtomicU64 = AtomicU64::new(0);

/// MMIO region for Local APIC registers.
/// Initialized once during detect() and used for all register access.
static APIC_REGS: OnceLock<MmioRegion> = OnceLock::new();

pub fn detect() -> bool {
    klog_debug!("APIC: Detecting Local APIC availability...");

    let (_, _, ecx, edx) = cpu::cpuid(CPUID_LEAF_FEATURES);
    if edx & CPUID_FEAT_EDX_APIC == 0 {
        klog_debug!("APIC: Local APIC is not available");
        APIC_AVAILABLE.reset();
        return false;
    }

    APIC_AVAILABLE.mark_set();
    if (ecx & CPUID_FEAT_ECX_X2APIC) != 0 {
        X2APIC_AVAILABLE.mark_set();
    }

    let apic_base_msr = cpu::read_msr(Msr::APIC_BASE);
    let apic_phys = apic_base_msr & ApicBaseMsr::ADDR_MASK;
    APIC_BASE_PHYSICAL.store(apic_phys, Ordering::Relaxed);

    let phys_addr = PhysAddr::new(apic_phys);
    match MmioRegion::map(phys_addr, APIC_REGION_SIZE) {
        Some(region) => {
            let virt = region.virt_base();
            APIC_REGS.call_once(|| region);

            let bsp_flag = if apic_base_msr & ApicBaseMsr::BSP != 0 {
                " BSP"
            } else {
                ""
            };
            let x2apic_flag = if apic_base_msr & ApicBaseMsr::X2APIC_ENABLE != 0 {
                " X2APIC"
            } else {
                ""
            };
            let enable_flag = if apic_base_msr & ApicBaseMsr::GLOBAL_ENABLE != 0 {
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
        }
        None => {
            klog_info!("APIC: ERROR - Failed to map APIC registers");
            APIC_AVAILABLE.reset();
            false
        }
    }
}

pub fn init<'b>(token: &slopos_ostd::sync::BspToken<'b>) -> i32 {
    if !is_available() {
        klog_info!("APIC: Cannot initialize - APIC not available");
        return -1;
    }

    klog_debug!("APIC: Initializing Local APIC");

    slopos_arch::pcr::register_lapic_id_fn(token, get_id);
    slopos_arch::pcr::register_send_ipi_to_cpu_fn(token, send_ipi_to_cpu);
    slopos_arch::pcr::register_send_nmi_fn(token, send_nmi_to);
    slopos_arch::pcr::register_send_nmi_broadcast_fn(token, send_nmi_all_excluding_self);

    let mut apic_base_msr = cpu::read_msr(Msr::APIC_BASE);
    if apic_base_msr & ApicBaseMsr::GLOBAL_ENABLE == 0 {
        apic_base_msr |= ApicBaseMsr::GLOBAL_ENABLE;
        cpu::write_msr(Msr::APIC_BASE, apic_base_msr);
        klog_debug!("APIC: Enabled APIC globally via MSR");
    }

    enable();

    write_register(LAPIC_LVT_TIMER, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_LINT0, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_LINT1, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_ERROR, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_PERFCNT, LAPIC_LVT_MASKED);

    write_register(LAPIC_LVT_LINT0, LAPIC_LVT_DELIVERY_MODE_EXTINT);

    write_register(LAPIC_ESR, 0);
    write_register(LAPIC_ESR, 0);

    send_eoi();

    let apic_id = get_id();
    let apic_version = get_version();
    klog_debug!("APIC: ID: 0x{:x}, Version: 0x{:x}", apic_id, apic_version);

    APIC_ENABLED.mark_set();
    klog_debug!("APIC: Initialization complete");
    0
}

pub fn is_available() -> bool {
    APIC_AVAILABLE.is_set_relaxed()
}

pub fn is_x2apic_available() -> bool {
    X2APIC_AVAILABLE.is_set_relaxed()
}

pub fn is_bsp() -> bool {
    if !is_available() {
        return false;
    }
    let apic_base_msr = cpu::read_msr(Msr::APIC_BASE);
    (apic_base_msr & ApicBaseMsr::BSP) != 0
}

pub fn is_enabled() -> bool {
    APIC_ENABLED.is_set_relaxed()
}

pub fn enable() {
    if !is_available() {
        return;
    }
    let mut spurious = read_register(LAPIC_SPURIOUS);
    spurious |= LAPIC_SPURIOUS_ENABLE;
    spurious |= 0xFF;
    write_register(LAPIC_SPURIOUS, spurious);
    APIC_ENABLED.mark_set();
    klog_debug!("APIC: Local APIC enabled");
}

pub fn disable() {
    if !is_available() {
        return;
    }
    let mut spurious = read_register(LAPIC_SPURIOUS);
    spurious &= !LAPIC_SPURIOUS_ENABLE;
    write_register(LAPIC_SPURIOUS, spurious);
    APIC_ENABLED.reset();
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

const IPI_POLL_LIMIT: u32 = 10_000;
const ICR_DEST_ALL_EXCLUDING_SELF: u32 = 0x3 << 18;

fn wait_icr_idle() {
    let mut remaining = IPI_POLL_LIMIT;
    while (read_register(LAPIC_ICR_LOW) & LAPIC_ICR_DELIVERY_STATUS) != 0 && remaining > 0 {
        cpu::pause();
        remaining -= 1;
    }
}

fn send_ipi_raw(icr_high: u32, icr_low: u32) {
    if !is_available() || !is_enabled() {
        return;
    }
    wait_icr_idle();
    write_register(LAPIC_ICR_HIGH, icr_high);
    write_register(LAPIC_ICR_LOW, icr_low);
    wait_icr_idle();
}

fn fixed_ipi_flags(vector: u32) -> u32 {
    vector
        | LAPIC_ICR_DELIVERY_FIXED
        | LAPIC_ICR_DEST_PHYSICAL
        | LAPIC_ICR_LEVEL_ASSERT
        | LAPIC_ICR_TRIGGER_EDGE
}

pub fn send_ipi_halt_all() {
    const SHUTDOWN_VECTOR: u32 = 0xFE;
    send_ipi_raw(LAPIC_ICR_DEST_BROADCAST, fixed_ipi_flags(SHUTDOWN_VECTOR));
    klog_debug!("APIC: Sent shutdown IPI to all processors");
}

pub fn get_base_address() -> u64 {
    APIC_REGS.get().map(|r| r.virt_base()).unwrap_or(0)
}

pub fn set_base_address(base: u64) {
    if !is_available() {
        return;
    }
    let masked_base = base & ApicBaseMsr::ADDR_MASK;
    let mut apic_base_msr = cpu::read_msr(Msr::APIC_BASE);
    apic_base_msr = (apic_base_msr & !ApicBaseMsr::ADDR_MASK) | masked_base;
    cpu::write_msr(Msr::APIC_BASE, apic_base_msr);

    APIC_BASE_PHYSICAL.store(masked_base, Ordering::Relaxed);
    // Note: APIC_REGS is initialized once during detect() and cannot be updated.
    // Changing the APIC base address at runtime requires re-initialization.
    klog_info!(
        "APIC: Base address changed to 0x{:x} - restart required for new mapping",
        masked_base
    );
}

pub fn read_register(reg: u32) -> u32 {
    if !is_available() {
        return 0;
    }
    APIC_REGS
        .get()
        .map(|r| r.read::<u32>(reg as usize))
        .unwrap_or(0)
}

pub fn write_register(reg: u32, value: u32) {
    if !is_available() {
        return;
    }
    if let Some(r) = APIC_REGS.get() {
        r.write::<u32>(reg as usize, value);
    }
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

pub fn send_ipi_all_excluding_self(vector: u8) {
    send_ipi_raw(
        0,
        fixed_ipi_flags(vector as u32) | ICR_DEST_ALL_EXCLUDING_SELF,
    );
}

pub fn send_ipi_to_cpu(target_apic_id: u32, vector: u8) {
    send_ipi_raw(target_apic_id << 24, fixed_ipi_flags(vector as u32));
}

/// Send a Non-Maskable Interrupt to a specific CPU via the LAPIC ICR.
///
/// Used by the NMI watchdog to interrupt a CPU that appears stuck with
/// interrupts disabled (e.g. deadlocked spinlock).
pub fn send_nmi_to(target_apic_id: u32) {
    // ICR format: delivery mode NMI (0b100 << 8), level assert, edge trigger,
    // physical destination.  No vector is needed for NMI delivery.
    const ICR_DELIVERY_NMI: u32 = 0b100 << 8;
    let icr_low = ICR_DELIVERY_NMI
        | LAPIC_ICR_DEST_PHYSICAL
        | LAPIC_ICR_LEVEL_ASSERT
        | LAPIC_ICR_TRIGGER_EDGE;
    send_ipi_raw(target_apic_id << 24, icr_low);
}

/// Broadcast a Non-Maskable Interrupt to all OTHER CPUs in one ICR write.
///
/// The Reliable Abort Core's stop-the-world: when a CPU wins the fatal-panic
/// election it NMIs every peer so they stop touching the console and release
/// their locks before it prints. NMI (not the maskable halt IPI) is required
/// because a wedged peer may be spinning with `IF=0` (e.g. inside a TLB
/// `wait_for_acks`), and only an NMI pierces that. Mirrors Linux
/// `crash_smp_send_stop` / FreeBSD `stop_cpus_hard`.
pub fn send_nmi_all_excluding_self() {
    const ICR_DELIVERY_NMI: u32 = 0b100 << 8;
    send_ipi_raw(
        0,
        ICR_DELIVERY_NMI
            | LAPIC_ICR_LEVEL_ASSERT
            | LAPIC_ICR_TRIGGER_EDGE
            | ICR_DEST_ALL_EXCLUDING_SELF,
    );
}
