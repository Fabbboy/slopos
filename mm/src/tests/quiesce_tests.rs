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

/// An epoch that never closes grows the quarantine until allocation fails, and
/// one that closes twice discharges a quarantine an epoch early.
///
/// A closure must therefore report the epoch that was current when it ran and
/// leave the counter exactly one higher. A peer's tick can close an epoch
/// inside either window, which is what the retry is for; an attempt that
/// brackets its own closure without one is the witness.
pub fn test_quiesce_epoch_advances() -> TestResult {
    let mut attempts = 0u32;
    let mut last = (0u64, 0u64, 0u64);
    while attempts < CLOSURE_BUDGET {
        let before = quiesce::current_epoch();
        let Some(closed) = quiesce::force_close_epoch_for_test() else {
            klog_info!("QUIESCE_TEST: no epoch closed under a full set of acks");
            return TestResult::Fail;
        };
        let after = quiesce::current_epoch();

        assert_test!(
            closed >= before,
            "closed epoch {} after the counter had already reached {}",
            closed,
            before
        );
        assert_test!(
            after > closed,
            "epoch {} closed but the counter never passed it",
            closed
        );
        if closed == before && after == closed + 1 {
            klog_info!("QUIESCE_TEST: closed epoch {}", closed);
            return TestResult::Pass;
        }

        last = (before, closed, after);
        attempts += 1;
    }

    klog_info!(
        "QUIESCE_TEST: {} attempts never bracketed one closure alone (last {} -> {} -> {})",
        attempts,
        last.0,
        last.1,
        last.2
    );
    TestResult::Fail
}

/// `quarantine_required` reads the epoch and the deferral stamp itself, so
/// pairing its answer with those two counters takes a sample that brackets the
/// call and agrees with itself — every CPU raises both.
fn sample_quarantine() -> Option<(u64, u64, bool)> {
    const SAMPLE_BUDGET: u32 = 64;

    for _ in 0..SAMPLE_BUDGET {
        let (epoch, _, stamp) = quiesce::stats();
        let required = quiesce::quarantine_required();
        let (epoch_again, _, stamp_again) = quiesce::stats();
        if epoch_again == epoch && stamp_again == stamp {
            return Some((epoch, stamp, required));
        }
    }
    None
}

/// A deferral in epoch `D` is discharged only at `D + 2`. A "deferred this
/// epoch?" flag reads false for a frame freed an epoch after its unmap — the
/// ordinary refcounted case — and releases it under a stale peer TLB.
pub fn test_quarantine_spans_two_epochs_after_a_deferred_unmap() -> TestResult {
    let deferred_at = quiesce::note_deferred_unmap();

    // The stamp is a `fetch_max`, so a peer's own deferral pushes the discharge
    // further out; re-reading it each round is what keeps this asserting the
    // window rather than a race against that peer.
    let mut closures = 0u32;
    loop {
        let Some((epoch, stamp, required)) = sample_quarantine() else {
            klog_info!("QUIESCE_TEST: epoch and deferral stamp never held still");
            return TestResult::Fail;
        };
        assert_test!(
            required == (epoch < stamp.saturating_add(2)),
            "epoch {} against the deferral stamped at {} (this test's unmap \
             stamped {}): quarantine_required() answered {} -- a false inside \
             the window is the use-after-free",
            epoch,
            stamp,
            deferred_at,
            required
        );
        if !required {
            return TestResult::Pass;
        }
        if closures >= CLOSURE_BUDGET {
            klog_info!(
                "QUIESCE_TEST: window still open after {} closures",
                closures
            );
            return TestResult::Fail;
        }
        if quiesce::force_close_epoch_for_test().is_none() {
            klog_info!("QUIESCE_TEST: no epoch closed under a full set of acks");
            return TestResult::Fail;
        }
        closures += 1;
    }
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
