//! Nanosecond-precision monotonic clock backed by the HPET main counter.
//!
//! Callable from any context; every accessor returns `0` before the platform
//! services are wired during early boot.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::platform;

static CACHED_TSC_HZ: AtomicU64 = AtomicU64::new(0);

#[inline]
fn tsc_frequency_hz() -> u64 {
    let cached = CACHED_TSC_HZ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }

    let (max_leaf, _, _, _) = slopos_ostd::arch::x86_64::cpuid::cpuid(0);
    let mut freq_hz = 3_000_000_000u64;
    if max_leaf >= 0x16 {
        let (base_mhz, _, _, _) = slopos_ostd::arch::x86_64::cpuid::cpuid(0x16);
        if base_mhz != 0 {
            freq_hz = (base_mhz as u64) * 1_000_000;
        }
    }

    CACHED_TSC_HZ.store(freq_hz, Ordering::Relaxed);
    freq_hz
}

#[inline]
pub fn monotonic_ns() -> u64 {
    platform::clock_monotonic_ns()
}

#[inline]
pub fn uptime_ms() -> u64 {
    monotonic_ns() / 1_000_000
}

/// `ticks` are TSC cycle deltas, as `kdiag_timestamp()` produces; the
/// conversion uses the CPUID-reported base frequency when available.
#[inline]
pub fn ticks_to_microseconds(ticks: u64) -> u64 {
    let freq_hz = tsc_frequency_hz();
    if freq_hz == 0 {
        return 0;
    }
    ((ticks as u128 * 1_000_000u128) / (freq_hz as u128)) as u64
}

// Coarse tick counter, incremented from the LAPIC timer arm in `boot/src/idt.rs`.
// Lives here so `slopos-core` and `slopos-sched` can both touch it without
// depending on each other.
static TIMER_TICK_COUNTER: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn get_timer_ticks() -> u64 {
    TIMER_TICK_COUNTER.load(Ordering::Relaxed)
}

#[inline]
pub fn increment_timer_ticks() {
    TIMER_TICK_COUNTER.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn reset_timer_ticks() {
    TIMER_TICK_COUNTER.store(0, Ordering::Relaxed);
}
