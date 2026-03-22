//! High-resolution monotonic clock.
//!
//! Provides nanosecond-precision system time via the HPET main counter,
//! replacing the coarse tick-counting approach from the PIT era.
//!
//! All functions are safe to call from any context (interrupt, kernel thread,
//! syscall handler). Before the platform services are wired during early boot,
//! every accessor returns `0`.

use core::sync::atomic::{AtomicU64, Ordering};
use slopos_kernel_services::platform;

static CACHED_TSC_HZ: AtomicU64 = AtomicU64::new(0);

#[inline]
fn tsc_frequency_hz() -> u64 {
    let cached = CACHED_TSC_HZ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }

    let (max_leaf, _, _, _) = slopos_arch::cpu::cpuid(0);
    let mut freq_hz = 3_000_000_000u64;
    if max_leaf >= 0x16 {
        let (base_mhz, _, _, _) = slopos_arch::cpu::cpuid(0x16);
        if base_mhz != 0 {
            freq_hz = (base_mhz as u64) * 1_000_000;
        }
    }

    CACHED_TSC_HZ.store(freq_hz, Ordering::Relaxed);
    freq_hz
}

/// Returns the monotonic clock value in nanoseconds since boot.
///
/// Reads the HPET main counter and converts to nanoseconds.
/// Falls back to tick-based approximation when HPET is unavailable.
/// Returns `0` if platform services are not yet initialized.
#[inline]
pub fn monotonic_ns() -> u64 {
    platform::clock_monotonic_ns()
}

/// Returns system uptime in milliseconds.
///
/// Convenience wrapper around [`monotonic_ns`] with millisecond granularity.
/// Replaces `irq_get_timer_ticks()` tick-counting for time queries.
#[inline]
pub fn uptime_ms() -> u64 {
    monotonic_ns() / 1_000_000
}

/// Convert raw timestamp ticks to microseconds.
///
/// `Task.total_runtime` stores deltas from `kdiag_timestamp()`, which currently
/// uses TSC cycle deltas. Convert cycles to microseconds from a CPUID-reported
/// base frequency when available.
#[inline]
pub fn ticks_to_microseconds(ticks: u64) -> u64 {
    let freq_hz = tsc_frequency_hz();
    if freq_hz == 0 {
        return 0;
    }
    ((ticks as u128 * 1_000_000u128) / (freq_hz as u128)) as u64
}
