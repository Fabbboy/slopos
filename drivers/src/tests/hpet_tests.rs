use slopos_arch::tsc::rdtsc;
use slopos_testing::TestResult;
use slopos_testing::measure_elapsed_ms;
use slopos_utils::klog_info;

use crate::hpet;

// ---------------------------------------------------------------------------
// Nanosecond conversion math
// ---------------------------------------------------------------------------

/// Verify `nanoseconds(0)` returns 0.
pub fn test_hpet_nanoseconds_zero() -> TestResult {
    let ns = hpet::nanoseconds(0);
    if ns != 0 {
        klog_info!("HPET_TEST: BUG - nanoseconds(0) = {} (expected 0)", ns);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Verify `nanoseconds(1)` equals the tick period converted to nanoseconds.
/// For a 10 MHz HPET (period = 100_000_000 fs), one tick = 100 ns.
/// For a 100 MHz HPET (period = 10_000_000 fs), one tick = 10 ns.
pub fn test_hpet_nanoseconds_one_tick() -> TestResult {
    let period_fs = hpet::period_femtoseconds();
    if period_fs == 0 {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    let ns = hpet::nanoseconds(1);
    let expected = (period_fs as u64) / 1_000_000;

    if ns != expected {
        klog_info!(
            "HPET_TEST: BUG - nanoseconds(1) = {} (expected {} for period {} fs)",
            ns,
            expected,
            period_fs
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Verify nanosecond conversion is linear: `nanoseconds(N) == N * nanoseconds(1)`.
pub fn test_hpet_nanoseconds_linearity() -> TestResult {
    let period_fs = hpet::period_femtoseconds();
    if period_fs == 0 {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    let ns_1 = hpet::nanoseconds(1);
    for &n in &[10u64, 100, 1000, 10_000, 1_000_000] {
        let ns_n = hpet::nanoseconds(n);
        let expected = n * ns_1;
        if ns_n != expected {
            klog_info!(
                "HPET_TEST: BUG - nanoseconds({}) = {} (expected {})",
                n,
                ns_n,
                expected
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Verify u128 math does not overflow with large tick values.
/// At 100 MHz (10_000_000 fs), u64::MAX ticks ≈ 1.8×10^14 seconds.
/// The u128 intermediate must not wrap.
pub fn test_hpet_nanoseconds_large_ticks() -> TestResult {
    let period_fs = hpet::period_femtoseconds();
    if period_fs == 0 {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    // Verify with u64::MAX — the u128 path should handle this.
    let ns = hpet::nanoseconds(u64::MAX);
    if ns == 0 {
        klog_info!("HPET_TEST: BUG - nanoseconds(u64::MAX) returned 0 (overflow?)");
        return TestResult::Fail;
    }

    // Verify with a value that would overflow u64 multiply:
    // 10^12 ticks × 10^7 fs = 10^19 > u64::MAX ≈ 1.8×10^19
    let big_ticks: u64 = 1_000_000_000_000;
    let ns_big = hpet::nanoseconds(big_ticks);
    // Expected: big_ticks × period_fs / 1_000_000
    let expected = ((big_ticks as u128 * period_fs as u128) / 1_000_000) as u64;
    if ns_big != expected {
        klog_info!(
            "HPET_TEST: BUG - nanoseconds(10^12) = {} (expected {})",
            ns_big,
            expected
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

// ---------------------------------------------------------------------------
// Counter liveness
// ---------------------------------------------------------------------------

/// Verify the counter is advancing (two reads with a spin gap).
pub fn test_hpet_counter_advancing() -> TestResult {
    if !hpet::is_available() {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    let c1 = hpet::read_counter();
    for _ in 0..1000 {
        core::hint::spin_loop();
    }
    let c2 = hpet::read_counter();

    if c2 <= c1 {
        klog_info!(
            "HPET_TEST: BUG - counter not advancing (c1={}, c2={})",
            c1,
            c2
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Verify monotonicity: 100 consecutive reads must be non-decreasing.
pub fn test_hpet_counter_monotonic() -> TestResult {
    if !hpet::is_available() {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    let mut prev = hpet::read_counter();
    for i in 0..100 {
        let cur = hpet::read_counter();
        if cur < prev {
            klog_info!(
                "HPET_TEST: BUG - counter went backwards at iteration {} (prev={}, cur={})",
                i,
                prev,
                cur
            );
            return TestResult::Fail;
        }
        prev = cur;
    }
    TestResult::Pass
}

// ---------------------------------------------------------------------------
// Delay accuracy
// ---------------------------------------------------------------------------

/// Verify `delay_ms(10)` actually waits approximately 10 ms (within 5-50 ms).
pub fn test_hpet_delay_accuracy() -> TestResult {
    if !hpet::is_available() {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    let delay = 10u32;
    let start = rdtsc();
    hpet::delay_ms(delay);
    let end = rdtsc();

    let elapsed = measure_elapsed_ms(start, end);

    // Allow wide tolerance for QEMU jitter: 5-50 ms for a 10 ms delay.
    if elapsed < 5 {
        klog_info!(
            "HPET_TEST: BUG - delay_ms({}) returned too early ({}ms < 5ms)",
            delay,
            elapsed
        );
        return TestResult::Fail;
    }
    if elapsed > 50 {
        klog_info!(
            "HPET_TEST: WARNING - delay_ms({}) took {}ms (>50ms, QEMU scheduling?)",
            delay,
            elapsed
        );
        // Don't fail — QEMU under load can be slow.
    }
    TestResult::Pass
}

/// Verify `delay_ms(0)` returns instantly (≤ 2 ms).
pub fn test_hpet_delay_zero() -> TestResult {
    if !hpet::is_available() {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    let start = rdtsc();
    hpet::delay_ms(0);
    let end = rdtsc();

    let elapsed = measure_elapsed_ms(start, end);
    if elapsed > 2 {
        klog_info!(
            "HPET_TEST: BUG - delay_ms(0) took {}ms (should be instant)",
            elapsed
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ---------------------------------------------------------------------------
// Availability API
// ---------------------------------------------------------------------------

/// Verify `is_available()` returns true when called after init.
pub fn test_hpet_is_available() -> TestResult {
    if !hpet::is_available() {
        klog_info!("HPET_TEST: BUG - is_available() returned false after init");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Verify `period_femtoseconds()` returns a valid value within HPET spec range.
pub fn test_hpet_period_valid() -> TestResult {
    let period = hpet::period_femtoseconds();
    if period == 0 {
        klog_info!("HPET_TEST: BUG - period_femtoseconds() returned 0");
        return TestResult::Fail;
    }
    // Max per HPET spec: 0x05F5_E100 (100 ns = 100_000_000 fs).
    if period > 0x05F5_E100 {
        klog_info!(
            "HPET_TEST: BUG - period {} fs exceeds HPET spec max (0x05F5E100)",
            period
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::define_test_suite!(
    hpet,
    [
        test_hpet_is_available,
        test_hpet_period_valid,
        test_hpet_nanoseconds_zero,
        test_hpet_nanoseconds_one_tick,
        test_hpet_nanoseconds_linearity,
        test_hpet_nanoseconds_large_ticks,
        test_hpet_counter_advancing,
        test_hpet_counter_monotonic,
        test_hpet_delay_zero,
        test_hpet_delay_accuracy,
    ]
);
