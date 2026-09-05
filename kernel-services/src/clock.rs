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

/// Wall-clock epoch: `CLOCK_REALTIME` at the instant `monotonic_ns()` read
/// [`REALTIME_BASE_MONO_NS`]. Zero until a boot step supplies one, which is
/// what [`realtime_ns`] reports as "no wall clock".
static REALTIME_EPOCH_NS: AtomicU64 = AtomicU64::new(0);
static REALTIME_BASE_MONO_NS: AtomicU64 = AtomicU64::new(0);

/// Anchor `CLOCK_REALTIME` to `unix_seconds`, read from the firmware RTC at
/// hand-off. Advances thereafter on the monotonic counter: the RTC is read
/// once and never again, so the two cannot drift apart mid-boot and no
/// timestamp this kernel writes can go backwards.
pub fn set_realtime_epoch(unix_seconds: u64) {
    let mono = monotonic_ns();
    let Some(epoch) = unix_seconds.checked_mul(1_000_000_000) else {
        return;
    };
    REALTIME_BASE_MONO_NS.store(mono, Ordering::Relaxed);
    REALTIME_EPOCH_NS.store(epoch, Ordering::Release);
}

/// Nanoseconds since the Unix epoch, or `None` when no wall clock was
/// established. A caller stamping a timestamp must treat `None` as "leave the
/// field alone" rather than as zero — an epoch-zero timestamp on disk is
/// indistinguishable from a file written in 1970.
pub fn realtime_ns() -> Option<u64> {
    let epoch = REALTIME_EPOCH_NS.load(Ordering::Acquire);
    if epoch == 0 {
        return None;
    }
    let base = REALTIME_BASE_MONO_NS.load(Ordering::Relaxed);
    Some(epoch.saturating_add(monotonic_ns().saturating_sub(base)))
}

/// Seconds since the Unix epoch, in the width every on-disk filesystem
/// timestamp this kernel writes uses. Saturates in 2106 rather than wrapping
/// to 1970.
pub fn realtime_unix_secs() -> Option<u32> {
    realtime_ns().map(|ns| u32::try_from(ns / 1_000_000_000).unwrap_or(u32::MAX))
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
