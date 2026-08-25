//! Reclaim: bounding holding time, not just acquisition.
//!
//! Nothing here may assert on pool sizes: once two epoch closures make a
//! parked frame releasable, any idle CPU's bottom half may splice it away.

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::frame::FrameAllocOptions;
use slopos_ostd::mm::reclaim;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::mmu::quiesce;
use crate::page_alloc::{
    FrameAccounting, RECLAIM_PROBE_NAME, alloc_kernel_page_with, frame_accounting, free_page_frame,
    quarantine_reclaim_asks, reclaim_probe_zero_asks,
};

/// No peer can move a parked block until two epoch closures later.
fn park_a_frame() -> Option<PhysAddr> {
    quiesce::note_deferred_unmap();
    let pa = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp());
    if pa.is_null() {
        return None;
    }
    free_page_frame(pa);
    Some(pa)
}

/// Reclaimers are registered, named, and answer without freeing anything.
pub fn test_reclaim_registrants_report_without_freeing() -> TestResult {
    let mut names = 0usize;
    let mut saw_probe = false;
    reclaim::for_each_reclaimer(|name, _| {
        assert!(!name.is_empty());
        saw_probe |= name == RECLAIM_PROBE_NAME;
        names += 1;
    });
    assert_test!(names > 0, "no reclaimer is registered");
    assert_test!(
        saw_probe,
        "the reclaim probe is not in the registry walk -- nothing downstream \
         of the quarantine is observed"
    );

    let Some(witness) = park_a_frame() else {
        return fail!("witness frame allocation");
    };
    let parked = frame_accounting(witness);
    assert_test!(
        parked == FrameAccounting::Quarantined,
        "the free left this test's frame {:?}, so there is nothing here a \
         side effect could release",
        parked
    );

    let reported = reclaim::reclaimable_pages();
    let after = frame_accounting(witness);
    assert_test!(
        after == FrameAccounting::Quarantined,
        "asking what is reclaimable moved this test's own frame to {:?} -- the \
         counting path must have no side effects",
        after
    );
    // Nothing is asserted about `reported` being non-zero: a quiesced machine
    // with a cold cache legitimately holds nothing back.
    let _ = reported;
    pass!()
}

pub fn test_reclaim_run_respects_its_bound() -> TestResult {
    let before = quarantine_reclaim_asks();
    assert_test!(reclaim::run(0) == 0, "run(0) freed pages");
    let asks = quarantine_reclaim_asks().wrapping_sub(before);
    assert_test!(asks == 0, "run(0) put {} asks to the quarantine", asks);

    for _ in 0..8 {
        if park_a_frame().is_none() {
            return fail!("witness frame allocation");
        }
    }
    // Two closures carry a batch into the releasable backlog, the only list
    // reclaim splices from.
    for _ in 0..2 {
        if quiesce::force_close_epoch_for_test().is_none() {
            return fail!("no epoch closed under a full set of acks");
        }
    }

    let before = quarantine_reclaim_asks();
    let freed = reclaim::run(1);
    let asks = quarantine_reclaim_asks().wrapping_sub(before);
    assert_test!(
        asks == 1,
        "run(1) put {} asks to the quarantine and came back with {} pages -- \
         one ask meets a one-page budget, so the rest came after it was met",
        asks,
        freed
    );

    let zero_asks = reclaim_probe_zero_asks();
    assert_test!(
        zero_asks == 0,
        "the reclaim tier has asked a registrant for zero pages {} times -- \
         run walked past a budget it had already met",
        zero_asks
    );
    pass!()
}

pub fn test_reclaim_returns_pages_to_the_buddy() -> TestResult {
    let Some(witness) = park_a_frame() else {
        return fail!("witness frame allocation");
    };
    let parked = frame_accounting(witness);
    assert_test!(
        parked == FrameAccounting::Quarantined,
        "the free left this test's frame {:?}, so no reclaim of it can be \
         observed",
        parked
    );

    // Two closures carry a parked block into the releasable backlog, the only
    // list reclaim splices from.
    for _ in 0..2 {
        if quiesce::force_close_epoch_for_test().is_none() {
            return fail!("no epoch closed under a full set of acks");
        }
    }

    // One page per pass: the quarantine's splice holds the buddy's
    // interrupts-off lock for as many blocks as it is asked for.
    const RELEASE_PASSES: u32 = 256;

    let mut freed_total = 0u32;
    let mut passes = 0u32;
    while passes < RELEASE_PASSES && frame_accounting(witness) == FrameAccounting::Quarantined {
        let freed = reclaim::run(1);
        if freed == 0 {
            break;
        }
        freed_total = freed_total.saturating_add(freed);
        passes += 1;
    }

    let after = frame_accounting(witness);
    assert_test!(
        after != FrameAccounting::Quarantined,
        "{} passes released {} pages and left this test's own frame parked",
        passes,
        freed_total
    );
    pass!()
}

slopos_testing::stest!(
    name = test_reclaim_registrants_report_without_freeing,
    suite = reclaim
);
slopos_testing::stest!(name = test_reclaim_run_respects_its_bound, suite = reclaim);
slopos_testing::stest!(
    name = test_reclaim_returns_pages_to_the_buddy,
    suite = reclaim
);
