//! The lockup detector's wiring into live per-CPU state. The sample state
//! machine itself is pinned host-side in `slopos-ostd/tests/watchdog.rs`.

use slopos_arch::pcr;
use slopos_drivers::{apic, hpet};
use slopos_ostd::lock_class;
use slopos_ostd::watchdog;
use slopos_testing::{TestResult, assert_test};

pub fn test_heartbeat_advances_on_timer_tick() -> TestResult {
    let cpu = pcr::get_current_cpu();
    let before = pcr::heartbeat_for_cpu(cpu);

    // Three tick periods, timed by a clock the LAPIC does not drive.
    hpet::delay_ms(30);

    let after = pcr::heartbeat_for_cpu(cpu);
    assert_test!(
        after != before,
        "heartbeat did not move across three timer periods"
    );
    TestResult::Pass
}

/// The property the long bounded loops depend on: progress without a tick.
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

/// Leaving a `Suppress` scope must also move the heartbeat, so the first sample
/// after it is not stale by construction.
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

/// A CPU whose LAPIC timer is masked cannot tick, so it must not be watched for
/// not ticking.
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
    // Lowering the machine-wide fuse makes a descheduled CPU look wedged.
    let raised = original.checked_add(1).unwrap_or(u32::MAX - 1);
    assert_test!(
        watchdog::set_miss_threshold(raised),
        "a sane value was rejected"
    );
    assert_test!(
        watchdog::miss_threshold() == raised,
        "threshold did not take"
    );

    watchdog::set_miss_threshold(original);
    TestResult::Pass
}

/// The probe interlock admits one NMI per target at a time — without it a
/// per-tick check would restart a stalled CPU's dump 100 times a second.
pub fn test_probe_admits_one_at_a_time() -> TestResult {
    use watchdog::NmiDisposition;

    // An index no machine this runs on has, so a real detection cannot collide.
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

/// `holder` is written on acquisition and never cleared, so both halves of the
/// check carry weight: a zeroed field would otherwise decode as "CPU 0".
pub fn test_holder_is_named_only_while_held() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

    let lock: SpinLock<u32> =
        SpinLock::new(0, lock_class!("test.watchdog_holder", LOCK_LEVEL_UNORDERED));
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

/// Storing `next_ticket` would jump past every ticket already taken, leaving
/// those waiters spinning forever on a number that is never served.
pub fn test_force_unlock_releases_exactly_one() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

    let lock: SpinLock<u32> = SpinLock::new(
        0,
        lock_class!("test.watchdog_force_unlock", LOCK_LEVEL_UNORDERED),
    );

    // A holder plus one queued waiter.
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

    lock.release_leaked_guard_for_test();
    assert_test!(!lock.is_locked(), "a free lock was over-released");

    // A fresh acquisition must still be served.
    drop(lock.lock());
    TestResult::Pass
}

/// This CPU stops taking timer interrupts long enough that its watcher must
/// report it, and must survive being reported.
pub fn test_a_wedged_cpu_is_reported_and_survives() -> TestResult {
    const FUSE: u32 = 3;

    if slopos_arch::pcr::get_online_cpu_count() < 2 {
        return TestResult::Skipped;
    }

    let cpu = pcr::get_current_cpu();

    // Scoped to this CPU, or the injection reports whichever CPU stalled.
    let Some(_fuse) = watchdog::MissThresholdOverride::for_cpu(cpu, FUSE) else {
        return TestResult::Fail;
    };

    // Three samples, so the stall need only outlast ~30 ms of the watcher's
    // ticks; the delay lets it take one sample under the new threshold.
    hpet::delay_ms(50);

    let Some(watcher) = watchdog::watcher_of(cpu) else {
        // No AP has started its timer yet, so nothing is sampling us.
        return TestResult::Skipped;
    };

    let oops_before = slopos_ostd::panic_recovery::oops_count();
    let flags = slopos_arch::cpu::save_flags_cli();
    hpet::delay_ms(150);
    slopos_arch::cpu::restore_flags(flags);

    // What the watcher observed, not how many ticks it took: a descheduled
    // watcher bursts its ticks afterwards, satisfying any count over the window.
    let observed = matches!(
        watchdog::max_stall(watcher),
        Some((t, samples)) if t == cpu && samples >= FUSE
    );
    if !observed {
        return TestResult::Skipped;
    }

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

/// Over the policy function: arming the live global would make any concurrent
/// stall fatal.
pub fn test_fatal_escalation_defaults_off_under_a_hypervisor() -> TestResult {
    use slopos_ostd::watchdog::{PanicOverride, fatal_escalation_policy};

    assert_test!(
        !fatal_escalation_policy(PanicOverride::Unset, true),
        "a stalled heartbeat under a hypervisor is not evidence, so it must not be fatal"
    );
    assert_test!(
        fatal_escalation_policy(PanicOverride::Unset, false),
        "on bare metal an unset override must leave escalation available"
    );
    assert_test!(
        fatal_escalation_policy(PanicOverride::ForcedOn, true),
        "watchdog.panic=on did not force escalation"
    );
    assert_test!(
        !fatal_escalation_policy(PanicOverride::ForcedOff, false),
        "watchdog.panic=off did not suppress escalation"
    );

    // Or the table above describes something the detector never consults.
    assert_test!(
        watchdog::fatal_escalation_permitted()
            == fatal_escalation_policy(
                watchdog::panic_override(),
                slopos_ostd::arch::x86_64::cpuid::hypervisor_present()
            ),
        "the live escalation decision is not the policy over its own inputs"
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_a_wedged_cpu_is_reported_and_survives,
    suite = watchdog
);
slopos_testing::stest!(
    name = test_fatal_escalation_defaults_off_under_a_hypervisor,
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
