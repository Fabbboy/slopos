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

/// A lock names its holder only while it is held.
///
/// `holder` is written on acquisition and never cleared, so both halves of
/// the validation carry weight: an untaken lock's zeroed field would
/// otherwise decode as "CPU 0, ticket 0", and a released one would keep
/// naming whoever had it last.
pub fn test_holder_is_named_only_while_held() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

    let lock: SpinLock<u32> = SpinLock::new(0, LOCK_LEVEL_UNORDERED);
    let me = pcr::get_current_cpu();

    assert_test!(
        lock.holder_cpu_for_test().is_none(),
        "an untaken lock named a holder"
    );
    {
        let mut guard = lock.lock();
        *guard += 1;
        assert_test!(
            lock.holder_cpu_for_test() == Some(me),
            "a held lock did not name this CPU"
        );
    }
    assert_test!(
        lock.holder_cpu_for_test().is_none(),
        "a released lock still named a holder"
    );

    let guard = lock.try_lock().expect("uncontended try_lock must succeed");
    assert_test!(
        lock.holder_cpu_for_test() == Some(me),
        "try_lock left a hole in the wait-for graph"
    );
    drop(guard);
    assert_test!(
        lock.holder_cpu_for_test().is_none(),
        "a released try_lock still named a holder"
    );
    TestResult::Pass
}

/// A force-unlock releases one holder, not the whole queue.
///
/// Storing `next_ticket` would jump past every ticket already taken, and
/// each of those waiters would spin forever on a number that is never
/// served — manufacturing the lockup the detector exists to report.
pub fn test_force_unlock_releases_exactly_one() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

    let lock: SpinLock<u32> = SpinLock::new(0, LOCK_LEVEL_UNORDERED);

    // A holder plus one queued waiter. Storing `next_ticket` would free the
    // lock in one step and leave the waiter's ticket unreachable forever.
    lock.abandon_for_test();
    lock.abandon_for_test();
    assert_test!(lock.is_locked(), "two tickets taken but lock reads free");

    lock.release_leaked_guard_for_test();
    assert_test!(
        lock.is_locked(),
        "one release freed the lock and stranded the queued waiter"
    );

    lock.release_leaked_guard_for_test();
    assert_test!(!lock.is_locked(), "the queue did not drain");

    // Idempotent: a release on a free lock must not run `now_serving` ahead
    // of `next_ticket`, which would wedge every future acquirer.
    lock.release_leaked_guard_for_test();
    assert_test!(!lock.is_locked(), "a free lock was over-released");

    // The proof it is still usable: a fresh acquisition must be served.
    drop(lock.lock());
    TestResult::Pass
}

/// A real wedge is detected, reported, and survived.
///
/// This CPU stops taking timer interrupts for long enough that its watcher
/// must report it. The machine reaching the end of this function is the
/// assertion that matters: before the escalation ladder existed, every
/// detection ended in `panic!`, which is the force that made both of the
/// old thresholds grow until they stopped detecting anything.
pub fn test_a_wedged_cpu_is_reported_and_survives() -> TestResult {
    if slopos_arch::pcr::get_online_cpu_count() < 2 {
        // Nobody is watching, so there is nothing to observe.
        return TestResult::Skipped;
    }

    let cpu = pcr::get_current_cpu();
    let original = watchdog::miss_threshold();

    // Three samples, so the stall need only outlast ~30 ms of the watcher's
    // ticks rather than the full second the default asks for. The delay
    // lets the watcher take at least one sample under the new threshold —
    // it latches the report point when the heartbeat last moved.
    watchdog::set_miss_threshold(3);
    hpet::delay_ms(50);

    if watchdog::watcher_of(cpu).is_none() {
        watchdog::set_miss_threshold(original);
        // No AP has started its timer yet, so nothing is sampling us.
        return TestResult::Skipped;
    }

    let oops_before = slopos_ostd::panic_recovery::oops_count();
    let flags = slopos_arch::cpu::save_flags_cli();
    hpet::delay_ms(150);
    slopos_arch::cpu::restore_flags(flags);

    watchdog::set_miss_threshold(original);

    assert_test!(
        slopos_ostd::panic_recovery::oops_count() > oops_before,
        "a CPU that stopped ticking for 150 ms was never reported"
    );
    assert_test!(
        watchdog::probe_disposition(cpu) == watchdog::NmiDisposition::Unsolicited,
        "the handler did not release its probe"
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_a_wedged_cpu_is_reported_and_survives,
    suite = watchdog
);
slopos_testing::stest!(
    name = test_heartbeat_advances_on_timer_tick,
    suite = watchdog
);
slopos_testing::stest!(
    name = test_holder_is_named_only_while_held,
    suite = watchdog
);
slopos_testing::stest!(
    name = test_force_unlock_releases_exactly_one,
    suite = watchdog
);
slopos_testing::stest!(name = test_touch_advances_heartbeat, suite = watchdog);
slopos_testing::stest!(name = test_suppress_scopes_eligibility, suite = watchdog);
slopos_testing::stest!(name = test_masked_timer_is_not_watchable, suite = watchdog);
slopos_testing::stest!(name = test_miss_threshold_rejects_zero, suite = watchdog);
slopos_testing::stest!(name = test_probe_admits_one_at_a_time, suite = watchdog);
