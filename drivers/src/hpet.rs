//! HPET (High Precision Event Timer) driver.
//!
//! Primary monotonic time source and LAPIC-timer calibration reference. HPET is
//! mandatory: the kernel panics at boot if the ACPI HPET table is missing or the
//! hardware is unavailable.
//!
//! Init runs after IOAPIC setup. The main counter is safe to read from any CPU
//! without synchronization.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_acpi::hpet::Hpet;
use slopos_acpi::tables::AcpiTables;
use slopos_kernel_services::platform;
use slopos_mm::hhdm;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_ostd::sync::{InitFlag, OnceLock, StateFlag};
use slopos_ostd::{klog_debug, klog_info};

/// 64-bit RO. [63:32] CLK_PERIOD (fs), [15] LEG_RT_CAP, [13] COUNT_SIZE_CAP,
/// [12:8] NUM_TIM_CAP (timers-1), [7:0] REV_ID.
const REG_GENERAL_CAP: usize = 0x000;

/// 64-bit RW. [1] LEG_RT_CNF, [0] ENABLE_CNF.
const REG_GENERAL_CONFIG: usize = 0x010;

/// 64-bit RW. Monotonic; writes require the counter halted.
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

/// [`OnceLock`] so the `read_counter` lookup stays lock-free after a single
/// `Acquire` load.
static MMIO_REGION: OnceLock<MmioRegion> = OnceLock::new();

/// Returns `0` on success, `-1` on failure.
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

/// Returns `0` if the HPET is not initialised.
#[inline]
pub fn read_counter() -> u64 {
    match MMIO_REGION.get() {
        Some(mmio) => mmio.read::<u64>(REG_MAIN_COUNTER),
        None => 0,
    }
}

#[inline]
pub fn nanoseconds(ticks: u64) -> u64 {
    let period = PERIOD_FS.load(Ordering::Relaxed) as u64;
    if period == 0 {
        return 0;
    }
    ((ticks as u128 * period as u128) / 1_000_000) as u64
}

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

#[inline]
pub fn delay_ms(ms: u32) {
    delay_ns(ms as u64 * 1_000_000);
}

#[inline]
pub fn is_available() -> bool {
    HPET_READY.is_set()
}

/// `0` if the HPET is unavailable.
#[inline]
pub fn period_femtoseconds() -> u32 {
    PERIOD_FS.load(Ordering::Relaxed)
}

// TODO(tech-debt): identical to `period_femtoseconds` — collapse to one name.
#[inline]
pub fn period_fs() -> u32 {
    PERIOD_FS.load(Ordering::Relaxed)
}

/// `None` when the HPET is unavailable.
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
