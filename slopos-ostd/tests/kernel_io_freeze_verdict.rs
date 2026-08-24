//! The freeze wait's policy, driven directly against a fake clock: the real
//! path needs an HPET and a live registry, so the discrimination this encodes
//! is only reachable here.

use slopos_ostd::sync::kernel_io_task::{FreezeWait, freeze_wait_verdict};

const WINDOW: u64 = 50;
const CAP: u64 = WINDOW * 8;

#[test]
fn completion_outranks_every_other_arm() {
    assert_eq!(
        freeze_wait_verdict(0, 3, 0, 0, WINDOW, CAP),
        FreezeWait::Done
    );
    assert_eq!(
        freeze_wait_verdict(0, 3, WINDOW, CAP, WINDOW, CAP),
        FreezeWait::Done,
        "a wait that completed exactly at the cap is complete, not capped"
    );
    assert_eq!(
        freeze_wait_verdict(0, 3, WINDOW * 9, CAP * 4, WINDOW, CAP),
        FreezeWait::Done
    );
}

#[test]
fn an_open_window_polls_whatever_the_pending_delta() {
    for (now, start) in [(3usize, 3usize), (2, 3), (4, 3)] {
        assert_eq!(
            freeze_wait_verdict(now, start, WINDOW - 1, WINDOW - 1, WINDOW, CAP),
            FreezeWait::Poll
        );
    }
}

#[test]
fn the_window_closes_on_arrival_and_gives_up_without_one() {
    assert_eq!(
        freeze_wait_verdict(2, 3, WINDOW, WINDOW, WINDOW, CAP),
        FreezeWait::Extend
    );
    assert_eq!(
        freeze_wait_verdict(3, 3, WINDOW, WINDOW, WINDOW, CAP),
        FreezeWait::GiveUpStalled
    );
}

/// The reported CI shape: one straggler, so there is no set-level progress to
/// extend on. This function gives up; the per-thread lap check is what decides
/// whether that give-up is a finding.
#[test]
fn a_lone_straggler_stalls_rather_than_extending() {
    assert_eq!(
        freeze_wait_verdict(1, 1, WINDOW, WINDOW, WINDOW, CAP),
        FreezeWait::GiveUpStalled
    );
}

#[test]
fn the_cap_outranks_continuing_progress() {
    assert_eq!(
        freeze_wait_verdict(1, 5, WINDOW, CAP, WINDOW, CAP),
        FreezeWait::GiveUpCapped
    );
}

/// Pending cannot rise within one held freeze, but the arm must not read a rise
/// as progress if it ever did.
#[test]
fn a_rising_pending_count_is_not_progress() {
    assert_eq!(
        freeze_wait_verdict(3, 2, WINDOW, WINDOW, WINDOW, CAP),
        FreezeWait::GiveUpStalled
    );
}

/// Drives the whole loop over a fake clock, one thread freezing per window.
#[test]
fn arrivals_extend_the_wait_and_it_ends_complete() {
    let mut pending = 8usize;
    let mut pending_at_window_start = pending;
    let mut clock = 0u64;
    let mut window_start = 0u64;

    let outcome = loop {
        let verdict = freeze_wait_verdict(
            pending,
            pending_at_window_start,
            clock - window_start,
            clock,
            WINDOW,
            CAP,
        );
        match verdict {
            FreezeWait::Poll => {
                clock += 1;
                // One thread reaches the gate just before each window closes.
                if clock - window_start == WINDOW - 1 && pending > 0 {
                    pending -= 1;
                }
            }
            FreezeWait::Extend => {
                window_start = clock;
                pending_at_window_start = pending;
            }
            other => break other,
        }
    };

    assert_eq!(outcome, FreezeWait::Done);
    assert!(
        clock < CAP,
        "8 arrivals must fit inside the cap, took {clock}"
    );
}

/// With nothing arriving the wait costs exactly one window, not the cap.
#[test]
fn a_silent_wait_costs_one_window() {
    let mut clock = 0u64;
    let outcome = loop {
        match freeze_wait_verdict(1, 1, clock, clock, WINDOW, CAP) {
            FreezeWait::Poll => clock += 1,
            other => break other,
        }
    };
    assert_eq!(outcome, FreezeWait::GiveUpStalled);
    assert_eq!(clock, WINDOW);
}
