//! LAPIC Timer calibration and configuration.
//!
//! Calibrates the LAPIC timer frequency using HPET (preferred) or PIT
//! (fallback) as a reference clock.  The calibrated frequency enables
//! precise periodic timer setup for scheduler preemption.
//!
//! The LAPIC timer counts down from an initial value at a rate determined
//! by the CPU bus clock divided by a configurable divisor (currently 16).
//! Because this frequency varies per machine, a reference timer must
//! measure the actual tick rate once at boot.  After calibration,
//! [`set_periodic_ms`] converts a desired millisecond interval to the
//! correct initial count.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::{klog_debug, klog_info};

use super::regs::*;
use super::{
    is_enabled, read_register, timer_get_current_count, timer_set_divisor, write_register,
};

// ---------------------------------------------------------------------------
// Static state
// ---------------------------------------------------------------------------

/// Calibrated LAPIC timer frequency in Hz (ticks per second at divisor 16).
static LAPIC_TIMER_FREQ_HZ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Calibration tunables
// ---------------------------------------------------------------------------

/// Number of measurement samples to average (reduces noise from QEMU jitter).
const CALIBRATION_SAMPLES: u32 = 3;

/// Duration of each measurement window in nanoseconds (10 ms).
const CALIBRATION_WINDOW_NS: u64 = 10_000_000;

/// Sanity bounds — warn (but accept) if outside this range.
const MIN_REASONABLE_FREQ_HZ: u64 = 1_000_000; // 1 MHz
const MAX_REASONABLE_FREQ_HZ: u64 = 10_000_000_000; // 10 GHz

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Calibrate the LAPIC timer against a reference clock.
///
/// Uses HPET if available, otherwise falls back to PIT polled delay.
/// Stores the result in [`LAPIC_TIMER_FREQ_HZ`] and returns the
/// frequency in Hz, or `0` on failure.
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

/// Configure the LAPIC timer in periodic mode at a given interval.
///
/// The timer will fire an interrupt on the specified IDT `vector` every
/// `ms` milliseconds.  Requires prior calibration via [`calibrate`].
///
/// Returns `true` on success, `false` if not calibrated or the computed
/// initial count is out of range.
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

    // count = freq_hz × ms / 1000, using u128 to avoid intermediate overflow.
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
    write_register(LAPIC_LVT_TIMER, lvt);
    write_register(LAPIC_TIMER_ICR, count as u32);

    klog_debug!(
        "APIC TIMER: Periodic mode — vector 0x{:x}, {}ms, count {}",
        vector,
        ms,
        count,
    );

    true
}

/// Configure the LAPIC timer in **one-shot** mode firing the IDT
/// `vector` after `ms` milliseconds, then naturally going idle.
/// Used by the tickless-idle path: before the per-CPU idle loop
/// HLTs, it consults the next sleep-queue deadline and arms a
/// one-shot for that deadline so a sleeping kernel task wakes on
/// time instead of waiting for the next periodic tick.
///
/// On every periodic-or-oneshot timer ISR, the scheduler restores
/// periodic mode via [`set_periodic_ms`] (see
/// `sched/src/scheduler.rs::scheduler_timer_tick`) so the system
/// converges back to the 100 Hz baseline whenever a tick fires.
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
    write_register(LAPIC_LVT_TIMER, lvt);
    write_register(LAPIC_TIMER_ICR, count as u32);
    true
}

/// Return the calibrated LAPIC timer frequency in Hz.
///
/// Returns `0` if calibration has not been performed yet.
#[inline]
pub fn frequency_hz() -> u64 {
    LAPIC_TIMER_FREQ_HZ.load(Ordering::Acquire)
}

/// Whether calibration has been performed successfully.
#[inline]
pub fn is_calibrated() -> bool {
    LAPIC_TIMER_FREQ_HZ.load(Ordering::Acquire) != 0
}

/// Mask the LAPIC timer LVT entry (suppress interrupts without stopping the counter).
#[inline]
pub fn mask() {
    if !is_enabled() {
        return;
    }
    let lvt = read_register(LAPIC_LVT_TIMER);
    write_register(LAPIC_LVT_TIMER, lvt | LAPIC_LVT_MASKED);
}

/// Unmask the LAPIC timer LVT entry (resume delivering interrupts).
#[inline]
pub fn unmask() {
    if !is_enabled() {
        return;
    }
    let lvt = read_register(LAPIC_LVT_TIMER);
    write_register(LAPIC_LVT_TIMER, lvt & !LAPIC_LVT_MASKED);
}

// ---------------------------------------------------------------------------
// Reference clock abstraction
// ---------------------------------------------------------------------------

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
                // `pit_poll_delay_ms` reads the hardware counter directly via
                // PIT_BASE_FREQUENCY_HZ, so it works even before `pit_init`.
                let ms = (CALIBRATION_WINDOW_NS / 1_000_000) as u32;
                crate::pit::pit_poll_delay_ms(ms.max(1));
            }
        }
    }

    /// The effective window duration in nanoseconds.
    ///
    /// PIT operates in whole-millisecond steps, so round to what we
    /// actually waited.
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

// ---------------------------------------------------------------------------
// Internal calibration logic
// ---------------------------------------------------------------------------

/// Perform a multi-sample calibration against the given reference clock.
///
/// Returns the measured frequency in Hz, or `0` if the counter did not
/// advance (hardware problem).
fn calibrate_against(reference: ReferenceTimer) -> u64 {
    let mut total_elapsed: u64 = 0;

    for _ in 0..CALIBRATION_SAMPLES {
        // Configure one-shot mode, masked (no interrupt fires during
        // calibration).  One-shot is the default when PERIODIC/TSC-deadline
        // bits are clear.
        write_register(LAPIC_LVT_TIMER, LAPIC_TIMER_ONESHOT | LAPIC_LVT_MASKED);
        timer_set_divisor(LAPIC_TIMER_DIV_16);

        // Load maximum initial count so we never underflow to zero.
        write_register(LAPIC_TIMER_ICR, 0xFFFF_FFFF);

        // Wait the reference window.
        reference.delay();

        // Elapsed = initial − remaining.
        let remaining = timer_get_current_count();
        let elapsed = 0xFFFF_FFFFu32.wrapping_sub(remaining);

        // Stop the timer to leave a clean state.
        write_register(LAPIC_TIMER_ICR, 0);

        total_elapsed += elapsed as u64;
    }

    let avg_elapsed = total_elapsed / CALIBRATION_SAMPLES as u64;
    let window_ns = reference.window_ns();
    if window_ns == 0 || avg_elapsed == 0 {
        return 0;
    }

    // freq = avg_elapsed / (window_ns / 1e9) = avg_elapsed × 1e9 / window_ns
    (avg_elapsed as u128 * 1_000_000_000 / window_ns as u128) as u64
}
