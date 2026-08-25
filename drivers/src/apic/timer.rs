//! LAPIC Timer calibration and configuration.
//!
//! The LAPIC timer counts down at the CPU bus clock divided by a divisor
//! (16 here), a rate that varies per machine, so HPET — or PIT as a fallback —
//! measures the actual tick rate once at boot.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::{klog_debug, klog_info};

use super::regs::*;
use super::{
    is_enabled, read_register, timer_get_current_count, timer_set_divisor, write_register,
};

/// Calibrated LAPIC timer frequency in Hz, at divisor 16.
static LAPIC_TIMER_FREQ_HZ: AtomicU64 = AtomicU64::new(0);

/// Measurement samples averaged, to damp QEMU jitter.
const CALIBRATION_SAMPLES: u32 = 3;

/// Duration of each measurement window in nanoseconds (10 ms).
const CALIBRATION_WINDOW_NS: u64 = 10_000_000;

/// Sanity bounds — warn, but accept, outside this range.
const MIN_REASONABLE_FREQ_HZ: u64 = 1_000_000; // 1 MHz
const MAX_REASONABLE_FREQ_HZ: u64 = 10_000_000_000; // 10 GHz

/// Calibrate the LAPIC timer against HPET, or PIT polled delay if HPET is
/// unavailable, and return the frequency in Hz, or `0` on failure.
pub fn calibrate() -> u64 {
    if !is_enabled() {
        klog_info!("APIC TIMER: Cannot calibrate — LAPIC not enabled");
        return 0;
    }

    let (freq, source) = if crate::hpet::is_available() {
        (calibrate_against(ReferenceTimer::Hpet), "HPET")
    } else {
        klog_info!("APIC TIMER: HPET unavailable, using PIT fallback");
        (calibrate_against(ReferenceTimer::Pit), "PIT")
    };

    if freq == 0 {
        klog_info!("APIC TIMER: Calibration failed — counter did not advance");
        return 0;
    }

    if freq < MIN_REASONABLE_FREQ_HZ || freq > MAX_REASONABLE_FREQ_HZ {
        klog_info!(
            "APIC TIMER: WARNING — {} Hz outside expected range [{}, {}]",
            freq,
            MIN_REASONABLE_FREQ_HZ,
            MAX_REASONABLE_FREQ_HZ,
        );
    }

    LAPIC_TIMER_FREQ_HZ.store(freq, Ordering::Release);

    let mhz = freq / 1_000_000;
    let khz_frac = (freq % 1_000_000) / 1_000;
    klog_info!(
        "APIC TIMER: Calibrated at {}.{:03} MHz ({} Hz, div 16, via {})",
        mhz,
        khz_frac,
        freq,
        source,
    );

    freq
}

/// Fire an interrupt on IDT `vector` every `ms` milliseconds.
///
/// Returns `false` if [`calibrate`] has not run or the computed initial count
/// does not fit a `u32`.
pub fn set_periodic_ms(vector: u8, ms: u32) -> bool {
    if !is_enabled() {
        klog_info!("APIC TIMER: Cannot set periodic — LAPIC not enabled");
        return false;
    }

    let freq = LAPIC_TIMER_FREQ_HZ.load(Ordering::Acquire);
    if freq == 0 {
        klog_info!("APIC TIMER: Cannot set periodic — not calibrated");
        return false;
    }

    if ms == 0 {
        klog_info!("APIC TIMER: Cannot set periodic — interval is 0");
        return false;
    }

    let count = (freq as u128 * ms as u128 / 1000) as u64;
    if count == 0 || count > u32::MAX as u64 {
        klog_info!(
            "APIC TIMER: Count {} out of u32 range for {}ms at {} Hz",
            count,
            ms,
            freq,
        );
        return false;
    }

    timer_set_divisor(LAPIC_TIMER_DIV_16);
    let lvt = (vector as u32) | LAPIC_TIMER_PERIODIC;
    write_timer_lvt(lvt);
    write_register(LAPIC_TIMER_ICR, count as u32);

    klog_debug!(
        "APIC TIMER: Periodic mode — vector 0x{:x}, {}ms, count {}",
        vector,
        ms,
        count,
    );

    true
}

/// Fire an interrupt on IDT `vector` once after `ms` milliseconds, then go
/// idle. The tickless-idle path arms this for the next sleep-queue deadline
/// before HLTing; every timer ISR restores periodic mode via
/// [`set_periodic_ms`], so the system converges back to the 100 Hz baseline.
///
/// Returns `false` if not calibrated, the count is out of u32
/// range, or the LAPIC is not enabled.
pub fn set_oneshot_ms(vector: u8, ms: u32) -> bool {
    if !is_enabled() {
        return false;
    }
    let freq = LAPIC_TIMER_FREQ_HZ.load(Ordering::Acquire);
    if freq == 0 || ms == 0 {
        return false;
    }
    let count = (freq as u128 * ms as u128 / 1000) as u64;
    if count == 0 || count > u32::MAX as u64 {
        return false;
    }

    timer_set_divisor(LAPIC_TIMER_DIV_16);
    let lvt = (vector as u32) | LAPIC_TIMER_ONESHOT;
    write_timer_lvt(lvt);
    write_register(LAPIC_TIMER_ICR, count as u32);
    true
}

/// Calibrated LAPIC timer frequency in Hz, or `0` if not yet calibrated.
#[inline]
pub fn frequency_hz() -> u64 {
    LAPIC_TIMER_FREQ_HZ.load(Ordering::Acquire)
}

#[inline]
pub fn is_calibrated() -> bool {
    LAPIC_TIMER_FREQ_HZ.load(Ordering::Acquire) != 0
}

/// Write the timer LVT and republish whether this CPU still receives periodic
/// ticks.
///
/// The lockup detector's progress counter moves on the timer interrupt, so a
/// CPU whose timer is masked, one-shot or mid-calibration must not be watched.
/// Deriving the flag from the register write means no caller has to remember.
fn write_timer_lvt(lvt: u32) {
    write_register(LAPIC_LVT_TIMER, lvt);
    let ticking = (lvt & LAPIC_TIMER_PERIODIC) != 0 && (lvt & LAPIC_LVT_MASKED) == 0;
    slopos_arch::pcr::set_timer_armed(ticking);
}

/// Mask the LAPIC timer LVT entry (suppress interrupts without stopping the counter).
#[inline]
pub fn mask() {
    if !is_enabled() {
        return;
    }
    let lvt = read_register(LAPIC_LVT_TIMER);
    write_timer_lvt(lvt | LAPIC_LVT_MASKED);
}

/// Unmask the LAPIC timer LVT entry.
#[inline]
pub fn unmask() {
    if !is_enabled() {
        return;
    }
    let lvt = read_register(LAPIC_LVT_TIMER);
    write_timer_lvt(lvt & !LAPIC_LVT_MASKED);
}

/// Selects which reference timer to use for the measurement window.
enum ReferenceTimer {
    /// HPET main counter — nanosecond-granularity delay.
    Hpet,
    /// PIT polled counter — millisecond-granularity fallback.
    Pit,
}

impl ReferenceTimer {
    /// Spin-wait for the calibration measurement window.
    fn delay(&self) {
        match self {
            Self::Hpet => crate::hpet::delay_ns(CALIBRATION_WINDOW_NS),
            Self::Pit => {
                // `pit_poll_delay_ms` reads the hardware counter directly, so
                // it works even before `pit_init`.
                let ms = (CALIBRATION_WINDOW_NS / 1_000_000) as u32;
                crate::pit::pit_poll_delay_ms(ms.max(1));
            }
        }
    }

    /// Effective window duration; PIT steps in whole milliseconds, so this
    /// rounds to what was actually waited.
    fn window_ns(&self) -> u64 {
        match self {
            Self::Hpet => CALIBRATION_WINDOW_NS,
            Self::Pit => {
                let ms = (CALIBRATION_WINDOW_NS / 1_000_000).max(1);
                ms * 1_000_000
            }
        }
    }
}

fn sample_window(reference: &ReferenceTimer) -> (u32, u64) {
    // Masked, so no interrupt fires during calibration.
    write_timer_lvt(LAPIC_TIMER_ONESHOT | LAPIC_LVT_MASKED);
    timer_set_divisor(LAPIC_TIMER_DIV_16);

    // Maximum initial count, so the counter cannot underflow to zero.
    write_register(LAPIC_TIMER_ICR, 0xFFFF_FFFF);

    let wall_start = crate::hpet::read_counter();
    reference.delay();
    let elapsed = 0xFFFF_FFFFu32.wrapping_sub(timer_get_current_count());
    let wall_ns = crate::hpet::nanoseconds(crate::hpet::read_counter().wrapping_sub(wall_start));

    // Writing 0 to the initial count register stops the timer.
    write_register(LAPIC_TIMER_ICR, 0);

    (elapsed, wall_ns)
}

/// Multi-sample calibration against `reference`, in Hz, or `0` if the counter
/// did not advance.
fn calibrate_against(reference: ReferenceTimer) -> u64 {
    let mut total_elapsed: u64 = 0;

    for _ in 0..CALIBRATION_SAMPLES {
        let (elapsed, _) = sample_window(&reference);
        total_elapsed += elapsed as u64;
    }

    let avg_elapsed = total_elapsed / CALIBRATION_SAMPLES as u64;
    let window_ns = reference.window_ns();
    if window_ns == 0 || avg_elapsed == 0 {
        return 0;
    }

    (avg_elapsed as u128 * 1_000_000_000 / window_ns as u128) as u64
}

#[cfg(feature = "test-hooks")]
pub struct CalibrationSample {
    pub lapic_ticks: u32,
    pub requested_window_ns: u64,
    pub observed_window_ns: u64,
}

/// The LAPIC counter stops while the host has this vCPU descheduled; the HPET does not.
#[cfg(feature = "test-hooks")]
pub fn sample_hpet_window_for_test() -> Option<CalibrationSample> {
    if !is_enabled() || !crate::hpet::is_available() {
        return None;
    }
    let (lapic_ticks, observed_window_ns) = sample_window(&ReferenceTimer::Hpet);
    Some(CalibrationSample {
        lapic_ticks,
        requested_window_ns: CALIBRATION_WINDOW_NS,
        observed_window_ns,
    })
}
