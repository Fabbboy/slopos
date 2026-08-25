use slopos_ostd::klog_info;
use slopos_testing::TestResult;

use crate::hpet;

// A deschedule stretches any single measurement, so only the shortest round counts.
const TIMING_ROUNDS: usize = 5;

pub fn test_hpet_nanoseconds_zero() -> TestResult {
    let ns = hpet::nanoseconds(0);
    if ns != 0 {
        klog_info!("HPET_TEST: BUG - nanoseconds(0) = {} (expected 0)", ns);
        return TestResult::Fail;
    }
    TestResult::Pass
}

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

/// The u128 intermediate must not wrap.
pub fn test_hpet_nanoseconds_large_ticks() -> TestResult {
    let period_fs = hpet::period_femtoseconds();
    if period_fs == 0 {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    let ns = hpet::nanoseconds(u64::MAX);
    if ns == 0 {
        klog_info!("HPET_TEST: BUG - nanoseconds(u64::MAX) returned 0 (overflow?)");
        return TestResult::Fail;
    }

    // 10^12 ticks × 10^7 fs = 10^19, which overflows a u64 multiply.
    let big_ticks: u64 = 1_000_000_000_000;
    let ns_big = hpet::nanoseconds(big_ticks);
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

pub fn test_hpet_delay_accuracy() -> TestResult {
    if !hpet::is_available() {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    const DELAY_MS: u32 = 10;
    let Some(required) = hpet::ms_to_ticks(DELAY_MS) else {
        klog_info!("HPET_TEST: BUG - ms_to_ticks() returned None while the HPET is available");
        return TestResult::Fail;
    };

    let mut shortest = u64::MAX;
    for _ in 0..TIMING_ROUNDS {
        let start = hpet::read_counter();
        hpet::delay_ms(DELAY_MS);
        let elapsed = hpet::read_counter().wrapping_sub(start);
        if elapsed < required {
            klog_info!(
                "HPET_TEST: BUG - delay_ms({}) returned after {} ticks, short of the {} it owes",
                DELAY_MS,
                elapsed,
                required
            );
            return TestResult::Fail;
        }
        shortest = shortest.min(elapsed);
    }

    if shortest > required * 2 {
        klog_info!(
            "HPET_TEST: BUG - shortest of {} delay_ms({}) calls took {} ticks, over twice the {} requested",
            TIMING_ROUNDS,
            DELAY_MS,
            shortest,
            required
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_hpet_delay_zero() -> TestResult {
    if !hpet::is_available() {
        klog_info!("HPET_TEST: SKIP - HPET not initialized");
        return TestResult::Skipped;
    }

    let Some(one_ms) = hpet::ms_to_ticks(1) else {
        klog_info!("HPET_TEST: BUG - ms_to_ticks() returned None while the HPET is available");
        return TestResult::Fail;
    };

    let mut shortest = u64::MAX;
    for _ in 0..TIMING_ROUNDS {
        let start = hpet::read_counter();
        hpet::delay_ms(0);
        shortest = shortest.min(hpet::read_counter().wrapping_sub(start));
    }

    // 1 ms is the shortest wait `delay_ms` can express.
    if shortest >= one_ms {
        klog_info!(
            "HPET_TEST: BUG - delay_ms(0) waited {} ticks, a whole millisecond being {}",
            shortest,
            one_ms
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_hpet_is_available() -> TestResult {
    if !hpet::is_available() {
        klog_info!("HPET_TEST: BUG - is_available() returned false after init");
        return TestResult::Fail;
    }
    TestResult::Pass
}

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

slopos_testing::stest!(name = test_hpet_is_available, suite = hpet);
slopos_testing::stest!(name = test_hpet_period_valid, suite = hpet);
slopos_testing::stest!(name = test_hpet_nanoseconds_zero, suite = hpet);
slopos_testing::stest!(name = test_hpet_nanoseconds_one_tick, suite = hpet);
slopos_testing::stest!(name = test_hpet_nanoseconds_linearity, suite = hpet);
slopos_testing::stest!(name = test_hpet_nanoseconds_large_ticks, suite = hpet);
slopos_testing::stest!(name = test_hpet_counter_advancing, suite = hpet);
slopos_testing::stest!(name = test_hpet_counter_monotonic, suite = hpet);
slopos_testing::stest!(name = test_hpet_delay_zero, suite = hpet);
slopos_testing::stest!(name = test_hpet_delay_accuracy, suite = hpet);
