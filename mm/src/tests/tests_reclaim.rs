//! Reclaim: bounding holding time, not just acquisition.
//!
//! Pins the two properties that make the tier trustworthy: it actually frees
//! pages, and it refuses rather than blocks when it cannot.
//!
//! Both are asserted against a frame this file allocated and parked itself.
//! The pools the reclaimers report on are shared, so three other CPUs fill and
//! drain them while a test reads; a frame nobody else holds is the one thing
//! here whose accounting only this file moves.

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::frame::FrameAllocOptions;
use slopos_ostd::mm::reclaim;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::mmu::quiesce;
use crate::page_alloc::{
    FrameAccounting, alloc_kernel_page_with, frame_accounting, free_page_frame,
};

/// Allocate a frame and free it into the quarantine's incoming list. Release
/// reaches a block only two epoch closures after it is parked, so until then no
/// peer can move it.
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
    reclaim::for_each_reclaimer(|name, _| {
        assert!(!name.is_empty());
        names += 1;
    });
    assert_test!(names > 0, "no reclaimer is registered");

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

/// `run(0)` is a no-op, and `run` never returns more than it was asked for.
pub fn test_reclaim_run_respects_its_bound() -> TestResult {
    assert_test!(reclaim::run(0) == 0, "run(0) freed pages");

    let want = 4u32;
    let freed = reclaim::run(want);
    assert_test!(
        freed <= want,
        "asked for {} pages and got {} back",
        want,
        freed
    );
    pass!()
}

/// Reclaim releases what it reports.
///
/// Deliberately asserts nothing against the buddy's free count, nor against a
/// re-read of the reclaimers' own report: a reclaimed page goes to the TLB
/// quarantine first, and the pool it sits in is one every CPU adds to.
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

    // Two closures carry a parked block from `incoming` through `draining`
    // into the releasable backlog, which is the only list reclaim splices from.
    for _ in 0..2 {
        if quiesce::force_close_epoch_for_test().is_none() {
            return fail!("no epoch closed under a full set of acks");
        }
    }

    let freed = reclaim::run(u32::MAX);
    let after = frame_accounting(witness);
    assert_test!(
        after != FrameAccounting::Quarantined,
        "reclaim reported {} pages released and still left this test's own \
         frame parked",
        freed
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
