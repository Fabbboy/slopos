//! Regression tests for LAPIC timer calibration and configuration.
//!
//! These tests run after the full boot sequence, so the LAPIC timer has
//! already been calibrated by `BOOT_STEP_LAPIC_CALIBRATION`.

use slopos_arch::arch::idt::LAPIC_TIMER_VECTOR;
use slopos_arch::pcr;
use slopos_ostd::klog_info;
use slopos_ostd::sync::PreemptGuard;
use slopos_testing::TestResult;

use crate::apic;
use crate::apic::regs::{LAPIC_LVT_TIMER, LAPIC_TIMER_ICR};
use crate::hpet;

/// The baseline scheduler tick these tests program.
const PERIOD_MS: u32 = 10;

/// Failsafe for a timer that never fires. Not the interval under test: the
/// LAPIC counts down only while this vCPU executes, so no amount of wall time
/// bounds how long a given number of ticks takes.
const TICK_FAILSAFE_MS: u32 = 1000;

/// The LAPIC programming a test interrupts and must put back: it owns the CPU
/// it runs on, and the scheduler — plus every later test — needs its ticks.
struct TimerProgramming {
    lvt: u32,
    initial_count: u32,
}

fn save_timer_programming() -> TimerProgramming {
    TimerProgramming {
        lvt: apic::read_register(LAPIC_LVT_TIMER),
        initial_count: apic::read_register(LAPIC_TIMER_ICR),
    }
}

fn restore_timer_programming(saved: &TimerProgramming) {
    apic::write_register(LAPIC_LVT_TIMER, saved.lvt);
    apic::timer_start(saved.initial_count);
}

/// Timer interrupts taken by *this* CPU.
///
/// `irq_get_timer_ticks()` is one global counter every CPU's ISR bumps, so it
/// advances at `cpu_count` × the tick rate and says nothing about this CPU. The
/// PCR heartbeat moves once per timer interrupt, on the CPU that took it.
fn local_ticks() -> u64 {
    pcr::heartbeat_for_cpu(pcr::get_current_cpu())
}

/// Spin until this CPU has taken `want` timer interrupts, returning how many
/// arrived before `failsafe_ms` of wall time ran out.
fn await_local_ticks(want: u64, failsafe_ms: u32) -> u64 {
    let start = local_ticks();
    let Some(failsafe_ticks) = hpet::ms_to_ticks(failsafe_ms) else {
        return local_ticks().saturating_sub(start);
    };
    let wall_start = hpet::read_counter();
    loop {
        let observed = local_ticks().saturating_sub(start);
        if observed >= want {
            return observed;
        }
        if hpet::read_counter().wrapping_sub(wall_start) >= failsafe_ticks {
            return observed;
        }
        core::hint::spin_loop();
    }
}

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

    let _pinned = PreemptGuard::new();
    let saved = save_timer_programming();

    const ROUNDS: usize = 8;
    let mut best_clean = 0u64;
    let mut best_any = 0u64;
    for _ in 0..ROUNDS {
        let Some(sample) = apic::timer::sample_hpet_window_for_test() else {
            continue;
        };
        if sample.observed_window_ns == 0 {
            continue;
        }
        let hz =
            (sample.lapic_ticks as u128 * 1_000_000_000 / sample.observed_window_ns as u128) as u64;
        best_any = best_any.max(hz);
        // A host cannot shorten the window, so the overshoot is time this vCPU
        // spent descheduled — time the LAPIC spent stopped and the HPET did not.
        let stretch = sample.requested_window_ns / 8;
        if sample.observed_window_ns <= sample.requested_window_ns + stretch {
            best_clean = best_clean.max(hz);
        }
    }

    restore_timer_programming(&saved);

    let recalibrated = if best_clean != 0 {
        best_clean
    } else {
        best_any
    };
    if recalibrated == 0 {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - no re-calibration window advanced the counter in {} rounds",
            ROUNDS,
        );
        return TestResult::Fail;
    }

    // `original` is one boot-time window that may itself have read low, which
    // is indistinguishable from a faster LAPIC; only downward drift is evidence.
    let tolerance = original / 7; // ~14.3%
    if original.saturating_sub(recalibrated) > tolerance {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - re-calibration drifted too much (original={}, recalibrated={}, tolerance={})",
            original,
            recalibrated,
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

    let _pinned = PreemptGuard::new();
    let saved = save_timer_programming();

    // `set_periodic_ms` programs the LVT unmasked, so stop the timer and put
    // the previous programming back before anything can fire on 0xEF.
    let ok = apic::timer::set_periodic_ms(0xEF, PERIOD_MS);

    apic::timer_stop();
    restore_timer_programming(&saved);

    if !ok {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - set_periodic_ms(0xEF, {}) returned false",
            PERIOD_MS,
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_lapic_timer_stop_clears_counter() -> TestResult {
    if !apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }

    // Stop and read must reach the same LAPIC, so nothing may migrate this task
    // between them.
    let _pinned = PreemptGuard::new();
    let saved = save_timer_programming();

    apic::timer_stop();
    let count = apic::timer_get_current_count();

    restore_timer_programming(&saved);

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
    if !apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }
    if !apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - timer not calibrated");
        return TestResult::Skipped;
    }

    let _pinned = PreemptGuard::new();

    apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, PERIOD_MS);

    const WANTED: u64 = 5;
    let observed = await_local_ticks(WANTED, TICK_FAILSAFE_MS);
    if observed < WANTED {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - only {} of {} ticks arrived on this CPU within {} ms",
            observed,
            WANTED,
            TICK_FAILSAFE_MS,
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// CRITICAL: Always unmask at the end, or the kernel will hang.
pub fn test_lapic_timer_mask_suppresses_ticks() -> TestResult {
    if !apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }
    if !apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - timer not calibrated");
        return TestResult::Skipped;
    }

    const MASKED_WINDOW_MS: u32 = 50;

    let _pinned = PreemptGuard::new();
    apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, PERIOD_MS);

    apic::timer::mask();
    // Read after the mask, so a tick already latched when it was written lands
    // on this side of the window.
    let masked_from = local_ticks();
    hpet::delay_ms(MASKED_WINDOW_MS);
    let delta_masked = local_ticks().saturating_sub(masked_from);

    apic::timer::unmask();
    let delta_unmasked = await_local_ticks(1, TICK_FAILSAFE_MS);

    if delta_masked != 0 {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - mask did not suppress this CPU's ticks (delta_masked={})",
            delta_masked,
        );
        return TestResult::Fail;
    }

    if delta_unmasked == 0 {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - unmask did not resume ticks within {} ms",
            TICK_FAILSAFE_MS,
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
    if !apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }
    if !apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - timer not calibrated");
        return TestResult::Skipped;
    }

    const ROUNDS: usize = 3;
    const WINDOW_MS: u32 = 100;

    let _pinned = PreemptGuard::new();
    apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, PERIOD_MS);

    let programmed_hz = 1000 / PERIOD_MS as u64;
    let min_hz = programmed_hz / 2;
    let max_hz = programmed_hz * 2;

    // Being descheduled costs this CPU ticks over a window of wall time and can
    // never hand it extra, so the ceiling holds for every round while the floor
    // holds only for the best of them.
    let mut best_hz = 0u64;
    for _ in 0..ROUNDS {
        let wall_start = hpet::read_counter();
        let ticks_start = local_ticks();
        hpet::delay_ms(WINDOW_MS);
        let delta_ticks = local_ticks().saturating_sub(ticks_start);
        let elapsed_ns = hpet::nanoseconds(hpet::read_counter().wrapping_sub(wall_start));

        if elapsed_ns == 0 {
            klog_info!(
                "LAPIC_TIMER_TEST: BUG - HPET did not advance across a {} ms delay",
                WINDOW_MS,
            );
            return TestResult::Fail;
        }

        let observed_hz = (delta_ticks as u128 * 1_000_000_000 / elapsed_ns as u128) as u64;
        if observed_hz > max_hz {
            klog_info!(
                "LAPIC_TIMER_TEST: BUG - tick rate {} Hz above {} Hz (delta_ticks={}, elapsed_ns={})",
                observed_hz,
                max_hz,
                delta_ticks,
                elapsed_ns,
            );
            return TestResult::Fail;
        }
        best_hz = best_hz.max(observed_hz);
    }

    if best_hz < min_hz {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - best of {} tick-rate rounds was {} Hz, below {} Hz",
            ROUNDS,
            best_hz,
            min_hz,
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
