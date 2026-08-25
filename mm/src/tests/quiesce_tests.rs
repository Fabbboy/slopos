//! TLB-quiesce epoch and frame-quarantine tests.
//!
//! The protection window is measured from the unmap, not from whenever the
//! frame happens to be freed.

use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test};

use crate::mmu::quiesce;
use crate::page_alloc;

/// Bounds the loops below. Every iteration closes an epoch, and three CPUs
/// cannot keep the window open against that for long.
const CLOSURE_BUDGET: u32 = 8;

/// Everything else here is vacuous if the machinery never armed.
pub fn test_quiesce_is_active() -> TestResult {
    assert_test!(
        quiesce::is_active(),
        "quiesce must be armed once the timer is running"
    );
    TestResult::Pass
}

/// An epoch that never closes grows the quarantine until allocation fails.
pub fn test_quiesce_epoch_advances() -> TestResult {
    let Some(closed) = quiesce::force_close_epoch_for_test() else {
        klog_info!("QUIESCE_TEST: no epoch closed under a full set of acks");
        return TestResult::Fail;
    };
    klog_info!("QUIESCE_TEST: closed epoch {}", closed);
    assert_test!(
        quiesce::current_epoch() > closed,
        "epoch {} closed but the counter never passed it",
        closed
    );
    TestResult::Pass
}

/// A deferral in epoch `D` is discharged only at `D + 2`. A "deferred this
/// epoch?" flag reads false for a frame freed an epoch after its unmap — the
/// ordinary refcounted case — and releases it under a stale peer TLB.
pub fn test_quarantine_spans_two_epochs_after_a_deferred_unmap() -> TestResult {
    let deferred_at = quiesce::note_deferred_unmap();
    assert_test!(
        quiesce::quarantine_required(),
        "a frame freed in the epoch its unmap stamped ({}) must be quarantined",
        deferred_at
    );

    // The stamp is a `fetch_max`, so a peer's own deferral pushes the discharge
    // further out; re-reading it each round is what keeps this asserting the
    // window rather than a race against that peer.
    let mut closures = 0u32;
    loop {
        let (epoch, _, stamp) = quiesce::stats();
        if epoch >= stamp + 2 {
            break;
        }
        assert_test!(
            quiesce::quarantine_required(),
            "epoch {} is within two of the deferral stamped at {}, yet the \
             quarantine discharged -- this is the use-after-free",
            epoch,
            stamp
        );
        if quiesce::force_close_epoch_for_test().is_none() {
            klog_info!("QUIESCE_TEST: no epoch closed under a full set of acks");
            return TestResult::Fail;
        }
        closures += 1;
        if closures > CLOSURE_BUDGET {
            klog_info!(
                "QUIESCE_TEST: window still open after {} closures",
                closures
            );
            return TestResult::Fail;
        }
    }

    assert_test!(
        !quiesce::quarantine_required(),
        "two closed epochs must discharge the deferral"
    );
    TestResult::Pass
}

/// Rotation runs from a timer interrupt: a splicing rotation would hold the
/// allocator's cli-lock for O(blocks x free-list length) inside that handler.
pub fn test_quarantine_rotate_does_not_splice() -> TestResult {
    let released = page_alloc::quarantine_rotate();
    assert_eq_test!(
        released,
        0,
        "rotate must only move list heads, never release frames"
    );
    TestResult::Pass
}

/// A backlog that only grows is an out-of-memory bug with extra steps.
///
/// Peers park frames behind this loop, so "the backlog is empty afterwards" is
/// not a property of the kernel; that each bounded pass over a non-empty
/// backlog splices something back is.
pub fn test_quarantine_backlog_drains() -> TestResult {
    const BLOCKS_PER_PASS: u32 = 64;
    const MAX_PASSES: u32 = 64;

    let mut passes = 0u32;
    let mut released_total = 0u32;
    while passes < MAX_PASSES {
        let released = page_alloc::quarantine_release_some(BLOCKS_PER_PASS);
        if released == 0 {
            break;
        }
        released_total = released_total.saturating_add(released);
        passes += 1;
    }

    assert_test!(
        passes < MAX_PASSES,
        "{} bounded releases spliced {} frames back and the backlog still had \
         more -- release is not keeping up with its own splice",
        passes,
        released_total
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_quiesce_is_active, suite = quiesce);
slopos_testing::stest!(name = test_quiesce_epoch_advances, suite = quiesce);
slopos_testing::stest!(
    name = test_quarantine_spans_two_epochs_after_a_deferred_unmap,
    suite = quiesce
);
slopos_testing::stest!(
    name = test_quarantine_rotate_does_not_splice,
    suite = quiesce
);
slopos_testing::stest!(name = test_quarantine_backlog_drains, suite = quiesce);
