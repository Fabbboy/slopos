//! Tests for the data-driven timer wheel: schedule + `process_due` dispatch,
//! cancellation, the `MAX_TIMERS_PER_PROCESS` bound, and fast-forward across a
//! large clock jump.

use slopos_ostd::KBox;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use super::net_scope::NetTestScope;
use crate::clock::MockClock;
use crate::timer::{FiredTimer, MAX_TIMERS_PER_PROCESS, NetTimerWheel, TimerKind, TimerToken};

/// Arbitrary non-zero base so the mock clock is active (zero = passthrough).
const BASE_MS: u64 = 1_000;

/// `now_ms` is the whole stack's clock, so pinning it is only safe inside a
/// [`NetTestScope`]; callers must bind both halves or the live stack reads it.
fn fresh_wheel() -> (KBox<NetTimerWheel>, NetTestScope) {
    let scope = NetTestScope::enter_at_mock_ms(BASE_MS).expect("net scope");
    let wheel = KBox::try_init(NetTimerWheel::init()).expect("alloc");
    (wheel, scope)
}

/// For tests that schedule and cancel by token without ever reading the clock.
fn fresh_wheel_unpinned() -> KBox<NetTimerWheel> {
    KBox::try_init(NetTimerWheel::init()).expect("alloc")
}

fn count_kind(fired: &[FiredTimer], kind: TimerKind) -> usize {
    fired.iter().filter(|t| t.kind == kind).count()
}

fn find_by_key(fired: &[FiredTimer], key: u32) -> Option<&FiredTimer> {
    fired.iter().find(|t| t.key == key)
}

pub fn test_timer_schedule_and_fire() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    let _token = wheel.schedule(5, TimerKind::ArpExpire, 42);
    assert_eq_test!(wheel.pending_count(), 1, "one timer pending after schedule");

    MockClock::advance(4);
    assert_test!(
        wheel.process_due().is_empty(),
        "timer should not fire before deadline"
    );
    assert_eq_test!(
        wheel.pending_count(),
        1,
        "timer still pending before deadline"
    );

    MockClock::advance(1);
    let fired = wheel.process_due();
    assert_eq_test!(fired.len(), 1, "exactly one timer fires at deadline");
    assert_eq_test!(fired[0].kind, TimerKind::ArpExpire, "correct TimerKind");
    assert_eq_test!(fired[0].key, 42, "correct key");
    assert_eq_test!(wheel.pending_count(), 0, "no timers pending after fire");

    pass!()
}

pub fn test_timer_fires_correct_kind_and_key() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    wheel.schedule(2, TimerKind::ArpRetransmit, 100);
    wheel.schedule(3, TimerKind::TcpRetransmit, 200);
    wheel.schedule(3, TimerKind::TcpTimeWait, 300);
    assert_eq_test!(wheel.pending_count(), 3, "three timers pending");

    MockClock::advance(1);
    assert_test!(wheel.process_due().is_empty(), "nothing at +1ms");

    MockClock::advance(1);
    let fired = wheel.process_due();
    assert_eq_test!(fired.len(), 1, "one timer at +2ms");
    assert_eq_test!(
        fired[0].kind,
        TimerKind::ArpRetransmit,
        "ArpRetransmit at +2ms"
    );
    assert_eq_test!(fired[0].key, 100, "key 100 at +2ms");

    MockClock::advance(1);
    let fired = wheel.process_due();
    assert_eq_test!(fired.len(), 2, "two timers at +3ms");
    assert_eq_test!(
        count_kind(&fired, TimerKind::TcpRetransmit),
        1,
        "one TcpRetransmit"
    );
    assert_eq_test!(
        count_kind(&fired, TimerKind::TcpTimeWait),
        1,
        "one TcpTimeWait"
    );
    assert_test!(find_by_key(&fired, 200).is_some(), "key 200 present");
    assert_test!(find_by_key(&fired, 300).is_some(), "key 300 present");
    assert_eq_test!(wheel.pending_count(), 0, "all timers consumed");

    pass!()
}

pub fn test_timer_delay_zero_fires_immediately() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    wheel.schedule(0, TimerKind::TcpDelayedAck, 7);
    let fired = wheel.process_due();
    assert_eq_test!(fired.len(), 1, "delay=0 fires on next process_due");
    assert_eq_test!(fired[0].key, 7, "correct key");

    pass!()
}

pub fn test_timer_cancel_before_deadline() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    let token = wheel.schedule(5, TimerKind::ArpExpire, 42);
    assert_test!(wheel.cancel(token), "cancel returns true for pending timer");

    MockClock::advance(100);
    assert_test!(
        wheel.process_due().is_empty(),
        "cancelled timer does not fire"
    );
    assert_eq_test!(wheel.pending_count(), 0, "cancelled timer cleaned up");

    pass!()
}

pub fn test_timer_cancel_already_fired() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    let token = wheel.schedule(1, TimerKind::ArpRetransmit, 99);
    MockClock::advance(1);
    assert_eq_test!(wheel.process_due().len(), 1, "timer fires");

    assert_test!(
        !wheel.cancel(token),
        "cancel returns false for already-fired timer"
    );

    pass!()
}

pub fn test_timer_cancel_invalid_token() -> TestResult {
    let wheel = fresh_wheel_unpinned();
    assert_test!(
        !wheel.cancel(TimerToken::INVALID),
        "cancel(INVALID) returns false"
    );
    pass!()
}

pub fn test_timer_cancel_one_of_many() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    let t1 = wheel.schedule(3, TimerKind::ArpExpire, 10);
    let _t2 = wheel.schedule(3, TimerKind::TcpRetransmit, 20);
    let t3 = wheel.schedule(3, TimerKind::TcpTimeWait, 30);

    assert_test!(wheel.cancel(t1), "cancel t1");
    assert_test!(wheel.cancel(t3), "cancel t3");

    MockClock::advance(3);
    let fired = wheel.process_due();
    assert_eq_test!(fired.len(), 1, "only one timer fires");
    assert_eq_test!(fired[0].kind, TimerKind::TcpRetransmit, "correct kind");
    assert_eq_test!(fired[0].key, 20, "correct key");

    pass!()
}

pub fn test_timer_double_cancel() -> TestResult {
    let wheel = fresh_wheel_unpinned();

    let token = wheel.schedule(5, TimerKind::ArpExpire, 42);
    assert_test!(wheel.cancel(token), "first cancel succeeds");
    assert_test!(!wheel.cancel(token), "second cancel returns false");

    pass!()
}

pub fn test_timer_max_per_process_bound() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    let count = 64usize;
    for i in 0..count {
        wheel.schedule(1, TimerKind::ArpExpire, i as u32);
    }
    assert_eq_test!(
        wheel.pending_count(),
        count,
        "64 timers pending before fire"
    );

    MockClock::advance(1);

    let fired = wheel.process_due();
    assert_eq_test!(
        fired.len(),
        MAX_TIMERS_PER_PROCESS,
        "exactly MAX_TIMERS_PER_PROCESS fire on first call"
    );
    assert_eq_test!(
        wheel.pending_count(),
        count - MAX_TIMERS_PER_PROCESS,
        "remaining timers deferred"
    );

    // No clock advance: the deferred timers are already due.
    let fired2 = wheel.process_due();
    assert_eq_test!(
        fired2.len(),
        count - MAX_TIMERS_PER_PROCESS,
        "deferred timers fire on the next call"
    );
    assert_eq_test!(wheel.pending_count(), 0, "no timers remain");

    pass!()
}

pub fn test_timer_max_per_process_bound_exact() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    for i in 0..MAX_TIMERS_PER_PROCESS {
        wheel.schedule(1, TimerKind::TcpRetransmit, i as u32);
    }

    MockClock::advance(1);
    let fired = wheel.process_due();
    assert_eq_test!(
        fired.len(),
        MAX_TIMERS_PER_PROCESS,
        "exactly MAX fires when count == MAX"
    );
    assert_eq_test!(wheel.pending_count(), 0, "no deferral when at bound");

    pass!()
}

pub fn test_timer_empty_wheel_process() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    MockClock::advance(100);
    assert_test!(
        wheel.process_due().is_empty(),
        "empty wheel produces no fired timers"
    );

    pass!()
}

pub fn test_timer_fast_forward_fires_all() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    wheel.schedule(3, TimerKind::ArpExpire, 1);
    wheel.schedule(5, TimerKind::TcpRetransmit, 2);
    wheel.schedule(7, TimerKind::TcpTimeWait, 3);

    MockClock::advance(10);
    let fired = wheel.process_due();
    assert_eq_test!(fired.len(), 3, "all three timers fire after one big jump");
    assert_eq_test!(wheel.pending_count(), 0, "wheel drained");

    pass!()
}

pub fn test_timer_large_delay_not_dropped() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    // 5000 ms is far past a 256-tick catch-up window.
    wheel.schedule(5000, TimerKind::ReassemblyTimeout, 77);
    assert_eq_test!(wheel.pending_count(), 1, "timer is pending");

    MockClock::advance(4999);
    assert_test!(wheel.process_due().is_empty(), "not due 1 ms early");

    MockClock::advance(1);
    let fired = wheel.process_due();
    assert_eq_test!(fired.len(), 1, "large-delay timer fires at its deadline");
    assert_eq_test!(fired[0].key, 77, "correct key");
    assert_eq_test!(wheel.pending_count(), 0, "timer consumed");

    pass!()
}

pub fn test_timer_multiple_same_deadline() -> TestResult {
    let (wheel, _scope) = fresh_wheel();

    for i in 0..5 {
        wheel.schedule(10, TimerKind::TcpKeepalive, i);
    }
    assert_eq_test!(
        wheel.pending_count(),
        5,
        "5 timers pending at same deadline"
    );

    MockClock::advance(10);
    let fired = wheel.process_due();
    assert_eq_test!(fired.len(), 5, "all 5 fire at the same deadline");

    pass!()
}

pub fn test_timer_pending_count_with_cancels() -> TestResult {
    let wheel = fresh_wheel_unpinned();

    let t1 = wheel.schedule(5, TimerKind::ArpExpire, 1);
    let _t2 = wheel.schedule(5, TimerKind::ArpRetransmit, 2);
    let t3 = wheel.schedule(5, TimerKind::TcpTimeWait, 3);
    assert_eq_test!(wheel.pending_count(), 3, "3 pending");

    wheel.cancel(t1);
    assert_eq_test!(wheel.pending_count(), 2, "2 pending after cancel(t1)");

    wheel.cancel(t3);
    assert_eq_test!(wheel.pending_count(), 1, "1 pending after cancel(t3)");

    pass!()
}

slopos_testing::stest!(name = test_timer_schedule_and_fire, suite = timer);
slopos_testing::stest!(name = test_timer_fires_correct_kind_and_key, suite = timer);
slopos_testing::stest!(
    name = test_timer_delay_zero_fires_immediately,
    suite = timer
);
slopos_testing::stest!(name = test_timer_cancel_before_deadline, suite = timer);
slopos_testing::stest!(name = test_timer_cancel_already_fired, suite = timer);
slopos_testing::stest!(name = test_timer_cancel_invalid_token, suite = timer);
slopos_testing::stest!(name = test_timer_cancel_one_of_many, suite = timer);
slopos_testing::stest!(name = test_timer_double_cancel, suite = timer);
slopos_testing::stest!(name = test_timer_max_per_process_bound, suite = timer);
slopos_testing::stest!(name = test_timer_max_per_process_bound_exact, suite = timer);
slopos_testing::stest!(name = test_timer_empty_wheel_process, suite = timer);
slopos_testing::stest!(name = test_timer_fast_forward_fires_all, suite = timer);
slopos_testing::stest!(name = test_timer_large_delay_not_dropped, suite = timer);
slopos_testing::stest!(name = test_timer_multiple_same_deadline, suite = timer);
slopos_testing::stest!(name = test_timer_pending_count_with_cancels, suite = timer);
