//! HPET (High Precision Event Timer) driver.
//!
//! Primary monotonic time source for SlopOS.  Provides LAPIC timer
//! calibration reference and nanosecond-precision polled delays.
//!
//! HPET is mandatory; the kernel panics at boot if
//! the ACPI HPET table is missing or the hardware is unavailable.
//!
//! Init after IOAPIC setup.  The main counter is safe to read from any
//! CPU without synchronization.  Init is guarded by [`InitFlag`] +
//! [`StateFlag`] for SMP safety.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_acpi::hpet::Hpet;
use slopos_acpi::tables::AcpiTables;
use slopos_kernel_services::platform;
use slopos_mm::hhdm;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_ostd::sync::{InitFlag, OnceLock, StateFlag};
use slopos_ostd::{klog_debug, klog_info};

/// General Capabilities and ID (64-bit RO).
/// [63:32] CLK_PERIOD (fs), [15] LEG_RT_CAP, [13] COUNT_SIZE_CAP,
/// [12:8] NUM_TIM_CAP (timers-1), [7:0] REV_ID.
const REG_GENERAL_CAP: usize = 0x000;

/// General Configuration (64-bit RW). [1] LEG_RT_CNF, [0] ENABLE_CNF.
const REG_GENERAL_CONFIG: usize = 0x010;

/// Main Counter Value (64-bit RW). Monotonic; writes require counter halted.
const REG_MAIN_COUNTER: usize = 0x0F0;

const CONFIG_ENABLE: u64 = 1 << 0;
/// Legacy replacement routing — disabled to avoid IOAPIC conflicts.
const CONFIG_LEGACY_REPLACE: u64 = 1 << 1;

const HPET_REGION_SIZE: usize = 0x400;

/// Max valid CLK_PERIOD per HPET spec: ≤ 100 ns (0x05F5_E100 fs).
const MAX_VALID_PERIOD_FS: u32 = 0x05F5_E100;

static HPET_READY: InitFlag = InitFlag::new();
static HPET_INIT_IN_PROGRESS: StateFlag = StateFlag::new();

/// Tick period in femtoseconds — cached for lock-free conversion.
static PERIOD_FS: AtomicU32 = AtomicU32::new(0);

/// MMIO region — cached for hot-path counter reads.  Wrapped in
/// [`OnceLock`] so the `read_counter` lookup is lock-free after a
/// single `Acquire` load.
static MMIO_REGION: OnceLock<MmioRegion> = OnceLock::new();

/// Initialize the HPET from ACPI tables.
///
/// Returns `0` on success, `-1` on failure.  The boot sequence treats
/// failure as fatal (panics) since HPET is mandatory.
pub fn init() -> i32 {
    if HPET_READY.is_set() {
        return 0;
    }
    if !HPET_INIT_IN_PROGRESS.enter() {
        while !HPET_READY.is_set() {
            core::hint::spin_loop();
        }
        return 0;
    }

    let result = init_inner();
    if result != 0 {
        HPET_INIT_IN_PROGRESS.leave();
    }
    result
}

/// Read the HPET main counter (64-bit monotonic). Returns `0` if not init'd.
#[inline]
pub fn read_counter() -> u64 {
    match MMIO_REGION.get() {
        Some(mmio) => mmio.read::<u64>(REG_MAIN_COUNTER),
        None => 0,
    }
}

/// Convert ticks to nanoseconds: `ns = ticks × period_fs / 1_000_000`.
/// Uses u128 to avoid overflow on large tick counts.
#[inline]
pub fn nanoseconds(ticks: u64) -> u64 {
    let period = PERIOD_FS.load(Ordering::Relaxed) as u64;
    if period == 0 {
        return 0;
    }
    ((ticks as u128 * period as u128) / 1_000_000) as u64
}

/// Spin-wait for the specified nanoseconds.
pub fn delay_ns(ns: u64) {
    let period = PERIOD_FS.load(Ordering::Relaxed) as u64;
    if period == 0 {
        return;
    }
    let ticks_needed = ((ns as u128 * 1_000_000) / period as u128) as u64;
    let start = read_counter();
    while read_counter().wrapping_sub(start) < ticks_needed {
        core::hint::spin_loop();
    }
}

/// Spin-wait for the specified milliseconds.
#[inline]
pub fn delay_ms(ms: u32) {
    delay_ns(ms as u64 * 1_000_000);
}

#[inline]
pub fn is_available() -> bool {
    HPET_READY.is_set()
}

/// Counter tick period in femtoseconds, or `0` if unavailable.
#[inline]
pub fn period_femtoseconds() -> u32 {
    PERIOD_FS.load(Ordering::Relaxed)
}

/// Return the HPET tick period in femtoseconds (0 if not initialized).
#[inline]
pub fn period_fs() -> u32 {
    PERIOD_FS.load(Ordering::Relaxed)
}

/// Convert milliseconds to HPET counter ticks. Returns `None` if HPET
/// is unavailable (period_fs == 0).
#[inline]
pub fn ms_to_ticks(ms: u32) -> Option<u64> {
    let period = period_fs() as u128;
    if period == 0 {
        return None;
    }
    Some(((ms as u128) * 1_000_000_000_000u128 / period) as u64)
}

fn init_inner() -> i32 {
    if !hhdm::is_available() {
        klog_info!("HPET: HHDM unavailable, cannot map MMIO registers");
        return -1;
    }

    if !platform::is_rsdp_available() {
        klog_info!("HPET: ACPI RSDP unavailable");
        return -1;
    }

    let Some(tables) = AcpiTables::from_phys(platform::get_rsdp_phys()) else {
        klog_info!("HPET: ACPI tables validation failed");
        return -1;
    };

    let Some(hpet_acpi) = Hpet::from_tables(&tables) else {
        klog_info!("HPET: ACPI HPET table not found or invalid");
        return -1;
    };

    let info = hpet_acpi.info();

    let Some(mmio) = MmioRegion::map(PhysAddr::new(info.base_phys), HPET_REGION_SIZE) else {
        klog_info!("HPET: Failed to map MMIO region at 0x{:x}", info.base_phys);
        return -1;
    };

    let cap: u64 = mmio.read::<u64>(REG_GENERAL_CAP);
    let period_fs = (cap >> 32) as u32;
    let rev_id = (cap & 0xFF) as u8;
    let num_timers = (((cap >> 8) & 0x1F) as u8).wrapping_add(1);
    let counter_64bit = (cap >> 13) & 1 != 0;

    if period_fs == 0 || period_fs > MAX_VALID_PERIOD_FS {
        klog_info!(
            "HPET: Invalid tick period {} fs (expected 1..{})",
            period_fs,
            MAX_VALID_PERIOD_FS
        );
        return -1;
    }

    // Halt counter → disable legacy routing → reset → enable.
    let mut config: u64 = mmio.read::<u64>(REG_GENERAL_CONFIG);
    config &= !CONFIG_ENABLE;
    config &= !CONFIG_LEGACY_REPLACE;
    mmio.write::<u64>(REG_GENERAL_CONFIG, config);

    mmio.write::<u64>(REG_MAIN_COUNTER, 0);

    config |= CONFIG_ENABLE;
    mmio.write::<u64>(REG_GENERAL_CONFIG, config);

    PERIOD_FS.store(period_fs, Ordering::Relaxed);
    MMIO_REGION.call_once(|| mmio);

    let freq_mhz = 1_000_000_000_000_000u64 / period_fs as u64 / 1_000_000;

    klog_info!(
        "HPET: base 0x{:x}, period {} fs (~{} MHz), {} comparators, {}-bit counter, rev {}",
        info.base_phys,
        period_fs,
        freq_mhz,
        num_timers,
        if counter_64bit { 64 } else { 32 },
        rev_id,
    );

    let c1 = read_counter();
    for _ in 0..1000 {
        core::hint::spin_loop();
    }
    let c2 = read_counter();
    if c2 <= c1 {
        klog_info!(
            "HPET: WARNING - main counter not advancing (c1={}, c2={})",
            c1,
            c2
        );
    } else {
        klog_debug!(
            "HPET: Counter advancing (delta {} ticks in ~1000 spins)",
            c2 - c1
        );
    }

    HPET_READY.mark_set();
    HPET_INIT_IN_PROGRESS.leave();
    0
}
