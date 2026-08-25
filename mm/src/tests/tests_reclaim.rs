//! Reclaim: bounding holding time, not just acquisition.
//!
//! Pins the three properties that make the tier trustworthy: it actually frees
//! pages, it refuses rather than blocks when it cannot, and it stops asking at
//! its budget.
//!
//! Nothing here may assert on the size of a pool. A frame this file parked is
//! its own until two epoch closures make it releasable; from that instant it
//! is anyone's, because every idle CPU's bottom half splices 64 pages off the
//! releasable backlog (`sched::runtime`). What stays this file's own is the
//! accounting of a specific frame, and the reclaim tier's own call counts.

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::frame::FrameAllocOptions;
use slopos_ostd::mm::reclaim;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::mmu::quiesce;
use crate::page_alloc::{
    FrameAccounting, alloc_kernel_page_with, frame_accounting, free_page_frame,
    quarantine_reclaim_asks,
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

/// `run(0)` reaches no registrant, and `run(1)` asks the first one once.
///
/// Not a page bound, and not the total either. `run` hands a registrant what
/// is *left* of the budget, so an ask made after the budget is met is an ask
/// for zero pages; and the only pool holding anything during a test — the
/// quarantine — is the first one asked. A `run` that dropped its budget checks
/// altogether therefore returns exactly what a correct one returns, which is
/// why the count of asks is the signal here and no arithmetic on `freed` is.
///
/// Frames are parked first so the one ask is a satisfied one rather than a
/// walk over an empty backlog. They are not asserted on: from the closure that
/// makes them releasable they are anyone's, and every idle CPU's bottom half
/// splices 64 pages off that backlog (`sched::runtime`).
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
    // Two closures carry the batch from `incoming` through `draining` into the
    // releasable backlog, the only list reclaim splices from.
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

    // One page per pass: `run` hands every registrant what is left of the
    // request, and the quarantine's splice holds the buddy's interrupts-off
    // lock for as many blocks as it is asked for. The witness sits behind
    // whatever peers parked after it, which is what the passes are for.
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
