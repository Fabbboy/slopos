//! TLB-quiesce epoch and frame-quarantine tests.
//!
//! The protection window is measured from the unmap, not from whenever the
//! frame happens to be freed.

use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test};

use crate::mmu::quiesce;
use crate::page_alloc;

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
    let (start, _, _) = quiesce::stats();
    quiesce::force_close_epoch_for_test();
    let (now, _, _) = quiesce::stats();
    klog_info!("QUIESCE_TEST: epoch {} -> {}", start, now);
    assert_eq_test!(
        now,
        start + 1,
        "a fully acked epoch must close exactly once"
    );
    TestResult::Pass
}

/// A deferral in epoch `D` is discharged only at `D + 2`. A "deferred this
/// epoch?" flag reads false for a frame freed an epoch after its unmap — the
/// ordinary refcounted case — and releases it under a stale peer TLB.
pub fn test_quarantine_spans_two_epochs_after_a_deferred_unmap() -> TestResult {
    quiesce::note_deferred_unmap();
    let (deferred_at, _, stamp) = quiesce::stats();
    assert_eq_test!(
        stamp,
        deferred_at,
        "note_deferred_unmap must stamp the current epoch"
    );
    assert_test!(
        quiesce::quarantine_required(),
        "a frame freed in the same epoch as the unmap must be quarantined"
    );

    // A peer may have acked early in `deferred_at`, so one closure is not enough.
    if !advance_one_epoch() {
        klog_info!("QUIESCE_TEST: could not close an epoch; environment too slow");
        return TestResult::Fail;
    }
    let (after_one, _, _) = quiesce::stats();
    assert_eq_test!(after_one, deferred_at + 1, "expected exactly one advance");
    assert_test!(
        quiesce::quarantine_required(),
        "one closed epoch does not discharge a deferral — this is the use-after-free"
    );

    if !advance_one_epoch() {
        klog_info!("QUIESCE_TEST: could not close the second epoch");
        return TestResult::Fail;
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
    let before = page_alloc::quarantine_frames();
    page_alloc::quarantine_rotate();
    let after = page_alloc::quarantine_frames();
    assert_eq_test!(
        after,
        before,
        "rotate must only move list heads, never release frames"
    );
    TestResult::Pass
}

/// A backlog that only grows is an out-of-memory bug with extra steps.
pub fn test_quarantine_backlog_drains() -> TestResult {
    for _ in 0..64 {
        if !page_alloc::quarantine_has_releasable() {
            break;
        }
        page_alloc::quarantine_release_some(64);
    }
    assert_test!(
        !page_alloc::quarantine_has_releasable(),
        "releasable backlog must drain under repeated bounded release"
    );
    TestResult::Pass
}

fn advance_one_epoch() -> bool {
    let (start, _, _) = quiesce::stats();
    quiesce::force_close_epoch_for_test();
    let (now, _, _) = quiesce::stats();
    now > start
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
