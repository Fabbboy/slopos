//! The lockup detector's wiring into live per-CPU state.
//!
//! The sample state machine is pinned host-side in
//! `slopos-ostd/tests/watchdog.rs`. What only a booted kernel can show is
//! that the heartbeat actually moves on a timer interrupt, that eligibility
//! tracks the LAPIC timer, and that suppression is honoured.

use slopos_arch::pcr;
use slopos_drivers::{apic, hpet};
use slopos_ostd::watchdog;
use slopos_testing::{TestResult, assert_test};

/// The timer interrupt advances this CPU's progress counter.
pub fn test_heartbeat_advances_on_timer_tick() -> TestResult {
    let cpu = pcr::get_current_cpu();
    let before = pcr::heartbeat_for_cpu(cpu);

    // Three periodic tick intervals, polled against a clock the LAPIC does
    // not drive.
    hpet::delay_ms(30);

    let after = pcr::heartbeat_for_cpu(cpu);
    assert_test!(
        after != before,
        "heartbeat did not move across three timer periods"
    );
    TestResult::Pass
}

/// `touch()` records progress without a timer interrupt — the property the
/// long bounded loops depend on.
pub fn test_touch_advances_heartbeat() -> TestResult {
    let cpu = pcr::get_current_cpu();
    let flags = slopos_arch::cpu::save_flags_cli();
    let before = pcr::heartbeat_for_cpu(cpu);
    watchdog::touch();
    let after = pcr::heartbeat_for_cpu(cpu);
    slopos_arch::cpu::restore_flags(flags);

    assert_test!(after != before, "touch did not move the heartbeat");
    TestResult::Pass
}

/// A `Suppress` token takes this CPU out of the watched set and puts it
/// back, leaving the heartbeat moved so the first sample after the scope
/// is not stale by construction.
pub fn test_suppress_scopes_eligibility() -> TestResult {
    let cpu = pcr::get_current_cpu();
    assert_test!(
        !pcr::watchdog_is_suppressed(cpu),
        "CPU was already suppressed before the scope"
    );

    let before = pcr::heartbeat_for_cpu(cpu);
    {
        let _quiet = watchdog::Suppress::for_current_cpu();
        assert_test!(
            pcr::watchdog_is_suppressed(cpu),
            "token did not suppress the CPU"
        );
    }

    assert_test!(
        !pcr::watchdog_is_suppressed(cpu),
        "suppression outlived its token"
    );
    assert_test!(
        pcr::heartbeat_for_cpu(cpu) != before,
        "leaving the scope did not record progress"
    );
    TestResult::Pass
}

/// Eligibility follows the hardware: a CPU whose LAPIC timer is masked
/// cannot tick, so it must not be watched for not ticking.
///
/// This is what keeps the LAPIC timer suite from being reported: those
/// tests stop and mask the timer on purpose, and leave it stopped across
/// the tests that follow.
pub fn test_masked_timer_is_not_watchable() -> TestResult {
    if !apic::timer::is_calibrated() {
        return TestResult::Skipped;
    }
    let cpu = pcr::get_current_cpu();
    assert_test!(
        pcr::timer_is_armed(cpu),
        "timer should be armed before masking it"
    );

    apic::timer::mask();
    let masked = pcr::timer_is_armed(cpu);
    apic::timer::unmask();
    let unmasked = pcr::timer_is_armed(cpu);

    assert_test!(!masked, "a masked timer still reported as armed");
    assert_test!(unmasked, "unmasking did not restore eligibility");
    TestResult::Pass
}

/// `watchdog.miss_threshold=0` is rejected: it would report every tick.
pub fn test_miss_threshold_rejects_zero() -> TestResult {
    let original = watchdog::miss_threshold();

    assert_test!(!watchdog::set_miss_threshold(0), "zero was accepted");
    assert_test!(
        watchdog::miss_threshold() == original,
        "a rejected value still changed the threshold"
    );
    assert_test!(watchdog::set_miss_threshold(7), "a sane value was rejected");
    assert_test!(watchdog::miss_threshold() == 7, "threshold did not take");

    watchdog::set_miss_threshold(original);
    TestResult::Pass
}

/// The probe interlock admits one NMI per target at a time — without it a
/// per-tick check would restart a stalled CPU's dump 100 times a second.
pub fn test_probe_admits_one_at_a_time() -> TestResult {
    use watchdog::NmiDisposition;

    // A CPU index no machine this runs on has, so a real detection cannot
    // collide with the assertions.
    const SPARE: usize = slopos_arch::MAX_CPUS - 1;
    watchdog::release_probe(SPARE);

    assert_test!(
        watchdog::arm_probe(SPARE, NmiDisposition::Report),
        "first arm was refused"
    );
    assert_test!(
        !watchdog::arm_probe(SPARE, NmiDisposition::Fatal),
        "a second probe was admitted while the first was in flight"
    );
    assert_test!(
        watchdog::probe_disposition(SPARE) == NmiDisposition::Report,
        "the losing arm overwrote the disposition"
    );

    watchdog::release_probe(SPARE);
    assert_test!(
        watchdog::probe_disposition(SPARE) == NmiDisposition::Unsolicited,
        "release did not free the slot"
    );
    assert_test!(
        watchdog::arm_probe(SPARE, NmiDisposition::Fatal),
        "slot did not become armable again"
    );
    watchdog::release_probe(SPARE);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_heartbeat_advances_on_timer_tick,
    suite = watchdog
);
slopos_testing::stest!(name = test_touch_advances_heartbeat, suite = watchdog);
slopos_testing::stest!(name = test_suppress_scopes_eligibility, suite = watchdog);
slopos_testing::stest!(name = test_masked_timer_is_not_watchable, suite = watchdog);
slopos_testing::stest!(name = test_miss_threshold_rejects_zero, suite = watchdog);
slopos_testing::stest!(name = test_probe_admits_one_at_a_time, suite = watchdog);
