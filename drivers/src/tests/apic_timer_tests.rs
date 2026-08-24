//! Regression tests for LAPIC timer calibration and configuration.
//!
//! These tests run after the full boot sequence, so the LAPIC timer has
//! already been calibrated by `BOOT_STEP_LAPIC_CALIBRATION`.

use slopos_arch::arch::idt::LAPIC_TIMER_VECTOR;
use slopos_arch::tsc::rdtsc;
use slopos_kernel_services::driver_runtime::irq_get_timer_ticks;
use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::measure_elapsed_ms;

use crate::apic;

pub fn test_lapic_timer_is_calibrated() -> TestResult {
    if !apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: BUG - is_calibrated() returned false after boot");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_lapic_timer_frequency_nonzero() -> TestResult {
    let freq = apic::timer::frequency_hz();
    if freq == 0 {
        klog_info!("LAPIC_TIMER_TEST: BUG - frequency_hz() returned 0 after calibration");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// With divisor 16, QEMU typically produces 50–500 MHz. The bounds are wider
/// (1 MHz – 10 GHz) to avoid false negatives on unusual hosts.
pub fn test_lapic_timer_frequency_in_range() -> TestResult {
    let freq = apic::timer::frequency_hz();
    if freq == 0 {
        klog_info!("LAPIC_TIMER_TEST: SKIP - not calibrated");
        return TestResult::Skipped;
    }

    const MIN_HZ: u64 = 1_000_000;
    const MAX_HZ: u64 = 10_000_000_000;

    if freq < MIN_HZ || freq > MAX_HZ {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - frequency {} Hz outside [{}, {}]",
            freq,
            MIN_HZ,
            MAX_HZ,
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Verifies the measurement is deterministic and not corrupted by stale state
/// left from the first calibration.
pub fn test_lapic_timer_recalibration_consistent() -> TestResult {
    let original = apic::timer::frequency_hz();
    if original == 0 {
        klog_info!("LAPIC_TIMER_TEST: SKIP - not calibrated");
        return TestResult::Skipped;
    }

    // Best of N rather than a wider tolerance: a calibration window the host
    // steals is a spoiled measurement, and the assertion should stay tight on a
    // clean one.
    const ROUNDS: usize = 5;
    let mut recalibrated = 0u64;
    let mut diff = u64::MAX;
    for _ in 0..ROUNDS {
        // Safe at runtime: one-shot masked mode, no interrupts fire.
        let sample = apic::timer::calibrate();
        if sample == 0 {
            klog_info!("LAPIC_TIMER_TEST: BUG - re-calibration returned 0");
            return TestResult::Fail;
        }
        let sample_diff = sample.abs_diff(original);
        if sample_diff < diff {
            diff = sample_diff;
            recalibrated = sample;
        }
    }

    let tolerance = original / 7; // ~14.3%

    if diff > tolerance {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - re-calibration drifted too much (original={}, recalibrated={}, diff={}, tolerance={})",
            original,
            recalibrated,
            diff,
            tolerance,
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_lapic_timer_periodic_zero_ms_rejected() -> TestResult {
    // Vector doesn't matter — the call bails before writing any register.
    let result = apic::timer::set_periodic_ms(0xEF, 0);
    if result {
        klog_info!("LAPIC_TIMER_TEST: BUG - set_periodic_ms(_, 0) returned true");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_lapic_timer_periodic_programs_timer() -> TestResult {
    if !apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - not calibrated");
        return TestResult::Skipped;
    }

    // `set_periodic_ms` programs the LVT unmasked, so stop the timer and
    // re-mask it (LVT timer register 0x320, bit 16) before anything can fire.
    let ok = apic::timer::set_periodic_ms(0xEF, 10);

    apic::timer_stop();
    apic::write_register(0x320, 1 << 16);

    if !ok {
        klog_info!("LAPIC_TIMER_TEST: BUG - set_periodic_ms(0xEF, 10) returned false");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_lapic_timer_stop_clears_counter() -> TestResult {
    if !apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }

    apic::timer_stop();
    let count = apic::timer_get_current_count();
    if count != 0 {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - timer_get_current_count() = {} after stop (expected 0)",
            count,
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// If ticks do not advance, the IDT handler is broken — e.g. a missing ISR
/// stub causing #GP on vector 0xEC.
pub fn test_lapic_timer_ticks_advance() -> TestResult {
    if !crate::apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }
    if !crate::apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - timer not calibrated");
        return TestResult::Skipped;
    }

    // Earlier tests leave the timer stopped and masked; restart it on the real
    // scheduler vector.
    crate::apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, 10);

    let ticks_before = irq_get_timer_ticks();

    // At 100 Hz, 100 ms is ~10 ticks; the floor of 5 absorbs QEMU jitter.
    crate::hpet::delay_ms(100);

    let ticks_after = irq_get_timer_ticks();
    let delta = ticks_after.saturating_sub(ticks_before);

    if delta < 5 {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - ticks barely advanced (before={}, after={}, delta={}, expected >=5)",
            ticks_before,
            ticks_after,
            delta,
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// CRITICAL: Always unmask at the end, or the kernel will hang.
pub fn test_lapic_timer_mask_suppresses_ticks() -> TestResult {
    if !crate::apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }
    if !crate::apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - timer not calibrated");
        return TestResult::Skipped;
    }

    crate::apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, 10);

    let ticks_before = irq_get_timer_ticks();

    crate::apic::timer::mask();

    crate::hpet::delay_ms(50);

    let ticks_after_mask = irq_get_timer_ticks();
    let delta_masked = ticks_after_mask.saturating_sub(ticks_before);

    crate::apic::timer::unmask();

    crate::hpet::delay_ms(50);

    let ticks_after_unmask = irq_get_timer_ticks();
    let delta_unmasked = ticks_after_unmask.saturating_sub(ticks_after_mask);

    // Other CPUs' LAPIC timers still fire into the global tick counter while
    // this CPU's is masked, hence the N*5 allowance over the masked window.
    let cpu_count = slopos_arch::pcr::get_cpu_count() as u64;
    let max_masked_ticks = if cpu_count > 1 { cpu_count * 5 } else { 1 };
    if delta_masked > max_masked_ticks {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - mask did not suppress ticks (delta_masked={}, max_allowed={})",
            delta_masked,
            max_masked_ticks,
        );
        return TestResult::Fail;
    }

    if delta_unmasked < 2 {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - unmask did not resume ticks (delta_unmasked={})",
            delta_unmasked,
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Documents the IDT gate contract.
pub fn test_lapic_timer_idt_gate_installed() -> TestResult {
    if LAPIC_TIMER_VECTOR != 0xEC {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - LAPIC_TIMER_VECTOR is {}, expected 0xEC",
            LAPIC_TIMER_VECTOR,
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_lapic_timer_tick_rate_reasonable() -> TestResult {
    if !crate::apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }
    if !crate::apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - timer not calibrated");
        return TestResult::Skipped;
    }

    crate::apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, 10);

    let ticks_before = irq_get_timer_ticks();
    let tsc_start = rdtsc();

    crate::hpet::delay_ms(200);

    let ticks_after = irq_get_timer_ticks();
    let tsc_end = rdtsc();

    let delta_ticks = ticks_after.saturating_sub(ticks_before);
    let elapsed_ms = measure_elapsed_ms(tsc_start, tsc_end);

    if elapsed_ms == 0 {
        klog_info!("LAPIC_TIMER_TEST: SKIP - elapsed_ms is 0");
        return TestResult::Skipped;
    }

    if delta_ticks == 0 {
        klog_info!("LAPIC_TIMER_TEST: BUG - no ticks advanced during 200ms measurement");
        return TestResult::Fail;
    }

    let observed_rate_hz = (delta_ticks as u64 * 1000) / (elapsed_ms as u64);

    // All CPUs' LAPIC timers increment the global tick counter, but earlier
    // tests may leave the AP timers stopped: hence a single-CPU floor and a
    // ceiling covering every CPU at 200 Hz.
    let cpu_count = slopos_arch::pcr::get_cpu_count() as u64;
    let min_hz: u64 = 50;
    let max_hz: u64 = 200 * cpu_count.max(1);

    if observed_rate_hz < min_hz || observed_rate_hz > max_hz {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - tick rate {} Hz outside [{}, {}] Hz (delta_ticks={}, elapsed_ms={})",
            observed_rate_hz,
            min_hz,
            max_hz,
            delta_ticks,
            elapsed_ms,
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

slopos_testing::stest!(name = test_lapic_timer_is_calibrated, suite = apic_timer);
slopos_testing::stest!(
    name = test_lapic_timer_frequency_nonzero,
    suite = apic_timer
);
slopos_testing::stest!(
    name = test_lapic_timer_frequency_in_range,
    suite = apic_timer
);
slopos_testing::stest!(
    name = test_lapic_timer_recalibration_consistent,
    suite = apic_timer
);
slopos_testing::stest!(
    name = test_lapic_timer_periodic_zero_ms_rejected,
    suite = apic_timer
);
slopos_testing::stest!(
    name = test_lapic_timer_periodic_programs_timer,
    suite = apic_timer
);
slopos_testing::stest!(
    name = test_lapic_timer_stop_clears_counter,
    suite = apic_timer
);
slopos_testing::stest!(name = test_lapic_timer_ticks_advance, suite = apic_timer);
slopos_testing::stest!(
    name = test_lapic_timer_mask_suppresses_ticks,
    suite = apic_timer
);
slopos_testing::stest!(
    name = test_lapic_timer_idt_gate_installed,
    suite = apic_timer
);
slopos_testing::stest!(
    name = test_lapic_timer_tick_rate_reasonable,
    suite = apic_timer
);
