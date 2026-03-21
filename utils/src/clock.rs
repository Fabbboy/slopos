//! High-resolution monotonic clock.
//!
//! Provides nanosecond-precision system time via the HPET main counter,
//! replacing the coarse tick-counting approach from the PIT era.
//!
//! All functions are safe to call from any context (interrupt, kernel thread,
//! syscall handler). Before the platform services are wired during early boot,
//! every accessor returns `0`.

use slopos_kernel_services::platform;

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
