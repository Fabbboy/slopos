//! The lockup detector's sample state machine.
//!
//! Each test owns a distinct watcher index: `SLOTS` is process-global and
//! `cargo test` runs integration tests on parallel threads, so sharing one
//! index would make the assertions race each other rather than the code.

use slopos_ostd::watchdog::test_support::{reset_slot, sample};

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

    // The predicate is inequality, not ordering: nothing about the detector
    // depends on the counter being monotonic, which is what keeps it free
    // of clock arithmetic.
    assert_eq!(sample(WATCHER, u64::MAX, THRESHOLD), 0);
    assert_eq!(sample(WATCHER, u64::MAX, THRESHOLD), 1);
    assert_eq!(sample(WATCHER, 0, THRESHOLD), 0);
}
