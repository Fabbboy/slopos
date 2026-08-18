//! The lockup detector's sample state machine. Each test must own a distinct
//! watcher index: `SLOTS` is process-global and `cargo test` runs these on
//! parallel threads.

use slopos_ostd::watchdog::max_stall;
use slopos_ostd::watchdog::test_support::{
    clear_snapshot, reset_slot, retarget, sample, sample_of,
};

const THRESHOLD: u32 = 4;

#[test]
fn a_moving_heartbeat_never_accumulates_staleness() {
    const WATCHER: usize = 10;
    reset_slot(WATCHER);

    for beat in 1..=20u64 {
        assert_eq!(
            sample(WATCHER, beat, THRESHOLD),
            0,
            "a heartbeat that moved must reset the counter"
        );
    }
}

#[test]
fn a_frozen_heartbeat_accumulates_one_per_sample() {
    const WATCHER: usize = 11;
    reset_slot(WATCHER);

    // First sight of a value is a change, so it resets.
    assert_eq!(sample(WATCHER, 7, THRESHOLD), 0);
    for expected in 1..=10u32 {
        assert_eq!(sample(WATCHER, 7, THRESHOLD), expected);
    }
}

#[test]
fn one_moving_sample_clears_an_accumulated_stall() {
    const WATCHER: usize = 12;
    reset_slot(WATCHER);

    assert_eq!(sample(WATCHER, 100, THRESHOLD), 0);
    for _ in 0..THRESHOLD * 3 {
        sample(WATCHER, 100, THRESHOLD);
    }
    assert_eq!(
        sample(WATCHER, 101, THRESHOLD),
        0,
        "a CPU that ticks once must be believed immediately"
    );
    assert_eq!(sample(WATCHER, 101, THRESHOLD), 1);
}

#[test]
fn a_heartbeat_that_wrapped_backwards_still_counts_as_progress() {
    const WATCHER: usize = 13;
    reset_slot(WATCHER);

    // The predicate is inequality, not ordering: the counter need not be
    // monotonic.
    assert_eq!(sample(WATCHER, u64::MAX, THRESHOLD), 0);
    assert_eq!(sample(WATCHER, u64::MAX, THRESHOLD), 1);
    assert_eq!(sample(WATCHER, 0, THRESHOLD), 0);
}

/// The worst stall names the CPU it was measured against, not whichever one the
/// watcher later moved on to. Retargeting is routine.
#[test]
fn a_recorded_maximum_survives_retargeting_without_changing_cpu() {
    const WATCHER: usize = 14;
    const MEASURED: usize = 5;
    const LATER: usize = 6;
    reset_slot(WATCHER);
    clear_snapshot();

    assert_eq!(sample_of(WATCHER, MEASURED, 42, THRESHOLD), 0);
    for _ in 0..3 {
        sample_of(WATCHER, MEASURED, 42, THRESHOLD);
    }
    assert_eq!(max_stall(WATCHER), Some((MEASURED, 3)));

    retarget(WATCHER, LATER, 99);
    assert_eq!(
        max_stall(WATCHER),
        Some((MEASURED, 3)),
        "the maximum was re-attributed to the new target"
    );

    assert_eq!(sample_of(WATCHER, LATER, 99, THRESHOLD), 1);
    assert_eq!(
        max_stall(WATCHER),
        Some((MEASURED, 3)),
        "a smaller stall on the new target overwrote the record"
    );

    for _ in 0..4 {
        sample_of(WATCHER, LATER, 99, THRESHOLD);
    }
    assert_eq!(
        max_stall(WATCHER),
        Some((LATER, 5)),
        "a larger stall did not take over the record"
    );
}

/// A watcher that has never seen a stall reports nothing, rather than
/// reporting zero samples against CPU 0.
#[test]
fn a_watcher_that_never_saw_a_stall_has_no_maximum() {
    const WATCHER: usize = 15;
    reset_slot(WATCHER);
    clear_snapshot();

    for beat in 1..=10u64 {
        sample_of(WATCHER, 3, beat, THRESHOLD);
    }
    assert_eq!(max_stall(WATCHER), None);
}
