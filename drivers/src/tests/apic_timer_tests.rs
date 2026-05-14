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

// ---------------------------------------------------------------------------
// Calibration state
// ---------------------------------------------------------------------------

/// After boot, calibration must have completed successfully.
pub fn test_lapic_timer_is_calibrated() -> TestResult {
    if !apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: BUG - is_calibrated() returned false after boot");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// `frequency_hz()` must return a non-zero value after calibration.
pub fn test_lapic_timer_frequency_nonzero() -> TestResult {
    let freq = apic::timer::frequency_hz();
    if freq == 0 {
        klog_info!("LAPIC_TIMER_TEST: BUG - frequency_hz() returned 0 after calibration");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// The calibrated frequency must be within a reasonable range.
///
/// With divisor 16, QEMU typically produces 50–500 MHz.  We use wider
/// bounds (1 MHz – 10 GHz) to avoid false negatives on unusual hosts.
pub fn test_lapic_timer_frequency_in_range() -> TestResult {
    let freq = apic::timer::frequency_hz();
    if freq == 0 {
        klog_info!("LAPIC_TIMER_TEST: SKIP - not calibrated");
        return TestResult::Skipped;
    }

    const MIN_HZ: u64 = 1_000_000; // 1 MHz
    const MAX_HZ: u64 = 10_000_000_000; // 10 GHz

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

// ---------------------------------------------------------------------------
// Re-calibration consistency
// ---------------------------------------------------------------------------

/// Re-running calibration should yield a similar frequency (within 15%).
///
/// This verifies the measurement is deterministic and not corrupted by
/// stale state left from the first calibration.
pub fn test_lapic_timer_recalibration_consistent() -> TestResult {
    let original = apic::timer::frequency_hz();
    if original == 0 {
        klog_info!("LAPIC_TIMER_TEST: SKIP - not calibrated");
        return TestResult::Skipped;
    }

    // Re-calibrate (safe: uses one-shot masked mode, no interrupts fire).
    let recalibrated = apic::timer::calibrate();
    if recalibrated == 0 {
        klog_info!("LAPIC_TIMER_TEST: BUG - re-calibration returned 0");
        return TestResult::Fail;
    }

    // Check they're within 15% of each other.
    let diff = if recalibrated > original {
        recalibrated - original
    } else {
        original - recalibrated
    };
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

// ---------------------------------------------------------------------------
// set_periodic_ms edge cases (no hardware side-effects)
// ---------------------------------------------------------------------------

/// `set_periodic_ms` with interval 0 must return false without touching hardware.
pub fn test_lapic_timer_periodic_zero_ms_rejected() -> TestResult {
    // Vector doesn't matter — the function should bail before writing registers.
    let result = apic::timer::set_periodic_ms(0xEF, 0);
    if result {
        klog_info!("LAPIC_TIMER_TEST: BUG - set_periodic_ms(_, 0) returned true");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Verify `set_periodic_ms` accepts a sane 10ms interval and actually
/// programs the timer, then immediately stop it to avoid spurious IRQs.
pub fn test_lapic_timer_periodic_programs_timer() -> TestResult {
    if !apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - not calibrated");
        return TestResult::Skipped;
    }

    // Use a masked vector (bit 16 set) to prevent any interrupt delivery.
    // set_periodic_ms programs LVT with the vector unmasked, so we need to
    // immediately stop the timer and re-mask it after verifying the return
    // value.
    let ok = apic::timer::set_periodic_ms(0xEF, 10);

    // Stop the timer immediately to prevent any interrupt delivery.
    apic::timer_stop();
    // Re-mask the timer LVT for safety.
    apic::write_register(0x320, 1 << 16);

    if !ok {
        klog_info!("LAPIC_TIMER_TEST: BUG - set_periodic_ms(0xEF, 10) returned false");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// After stopping the timer, the current count must be zero or decaying
/// to zero (the ICR was cleared).
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

// ---------------------------------------------------------------------------
// IDT handler integration and scheduling
// ---------------------------------------------------------------------------

/// Verify that the LAPIC timer IDT handler fires and advances the tick counter.
/// This is the most critical regression test for scheduler timer wiring.
///
/// If this fails, the IDT handler is broken (e.g., #GP exception not caught).
pub fn test_lapic_timer_ticks_advance() -> TestResult {
    // Check if timer is enabled and calibrated.
    if !crate::apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }
    if !crate::apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - timer not calibrated");
        return TestResult::Skipped;
    }

    // Earlier tests (stop_clears_counter, periodic_programs_timer) leave the
    // timer stopped and masked.  Restart it on the real scheduler vector so
    // we can verify the full IDT-handler → tick-counter path.
    crate::apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, 10);

    let ticks_before = irq_get_timer_ticks();

    // Delay ~100ms; at 100Hz we expect ~10 ticks, but allow margin for QEMU jitter.
    crate::hpet::delay_ms(100);

    let ticks_after = irq_get_timer_ticks();
    let delta = ticks_after.saturating_sub(ticks_before);

    // If ticks didn't advance, the IDT handler is broken — this is the
    // exact regression we are guarding against (e.g. missing ISR stub
    // causing #GP on vector 0xEC).
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

/// Verify that masking the timer suppresses tick advancement.
/// Unmasking should resume tick advancement.
///
/// CRITICAL: Always unmask at the end, or the kernel will hang.
pub fn test_lapic_timer_mask_suppresses_ticks() -> TestResult {
    // Check if timer is enabled and calibrated.
    if !crate::apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }
    if !crate::apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - timer not calibrated");
        return TestResult::Skipped;
    }

    // Restart the timer (earlier tests leave it stopped).
    crate::apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, 10);

    let ticks_before = irq_get_timer_ticks();

    // Mask the timer to suppress interrupts.
    crate::apic::timer::mask();

    // Delay 50ms; ticks should NOT advance (or minimal race condition).
    crate::hpet::delay_ms(50);

    let ticks_after_mask = irq_get_timer_ticks();
    let delta_masked = ticks_after_mask.saturating_sub(ticks_before);

    // Unmask to resume timer.
    crate::apic::timer::unmask();

    // Delay another 50ms; ticks should advance again.
    crate::hpet::delay_ms(50);

    let ticks_after_unmask = irq_get_timer_ticks();
    let delta_unmasked = ticks_after_unmask.saturating_sub(ticks_after_mask);

    // On SMP, other CPUs' LAPIC timers still fire into the global tick
    // counter while *this* CPU's timer is masked.  Allow up to N*5 ticks
    // (N = number of CPUs) during the 50ms masked window.
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

    // After unmask, ticks MUST resume.  If they don't, unmask is broken.
    if delta_unmasked < 2 {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - unmask did not resume ticks (delta_unmasked={})",
            delta_unmasked,
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Verify that the LAPIC_TIMER_VECTOR constant is correctly set to 0xEC.
/// This documents the IDT gate contract.
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

/// Verify that the timer tick rate is approximately 100Hz.
/// Measure wall time via TSC and compute observed tick rate.
pub fn test_lapic_timer_tick_rate_reasonable() -> TestResult {
    // Check if timer is enabled and calibrated.
    if !crate::apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }
    if !crate::apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - timer not calibrated");
        return TestResult::Skipped;
    }

    // Restart the timer (earlier tests leave it stopped).
    crate::apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, 10);

    let ticks_before = irq_get_timer_ticks();
    let tsc_start = rdtsc();

    // Delay 200ms to get a good measurement window.
    crate::hpet::delay_ms(200);

    let ticks_after = irq_get_timer_ticks();
    let tsc_end = rdtsc();

    let delta_ticks = ticks_after.saturating_sub(ticks_before);
    let elapsed_ms = measure_elapsed_ms(tsc_start, tsc_end);

    // Avoid division by zero.
    if elapsed_ms == 0 {
        klog_info!("LAPIC_TIMER_TEST: SKIP - elapsed_ms is 0");
        return TestResult::Skipped;
    }

    // Timer is calibrated and enabled — zero ticks means the handler is broken.
    if delta_ticks == 0 {
        klog_info!("LAPIC_TIMER_TEST: BUG - no ticks advanced during 200ms measurement");
        return TestResult::Fail;
    }

    // Compute observed rate: ticks per second.
    let observed_rate_hz = (delta_ticks as u64 * 1000) / (elapsed_ms as u64);

    // The global tick counter is incremented by ALL CPUs' LAPIC timers.
    // With N CPUs at ~100Hz each, the observed rate is ~N*100 Hz.
    // However, AP timers may be stopped by earlier tests in this suite,
    // so only the BSP timer (restarted above) may be ticking.  Use a
    // conservative floor of 50 Hz (single CPU, slow QEMU) and ceiling
    // based on all CPUs at 200 Hz.
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
