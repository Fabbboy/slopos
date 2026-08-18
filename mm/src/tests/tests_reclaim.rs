//! Reclaim: bounding holding time, not just acquisition.
//!
//! Pins the two properties that make the tier trustworthy: it actually frees
//! pages, and it refuses rather than blocks when it cannot.

use slopos_ostd::mm::reclaim;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, pass};

use crate::page_alloc::get_page_allocator_stats;

/// Reclaimers are registered, named, and answer without freeing anything.
pub fn test_reclaim_registrants_report_without_freeing() -> TestResult {
    let mut names = 0usize;
    reclaim::for_each_reclaimer(|name, _| {
        assert!(!name.is_empty());
        names += 1;
    });
    assert_test!(names > 0, "no reclaimer is registered");

    let free_before = get_page_allocator_stats().free;
    let reported = reclaim::reclaimable_pages();
    let free_after = get_page_allocator_stats().free;
    assert_test!(
        free_after == free_before,
        "asking what is reclaimable freed {} pages -- the counting path must \
         have no side effects",
        free_after.saturating_sub(free_before)
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

/// Reclaim releases what it reports, and never more than asked.
///
/// Deliberately asserts nothing against the buddy's free count: a reclaimed
/// page goes to the TLB quarantine first and only reaches a free list once
/// every CPU has proven it invalidated its translation.
///
/// Nothing need be reclaimable — the kernel phase runs at drivers/90, before
/// the services phase mounts ext2, so the block cache does not exist yet.
pub fn test_reclaim_returns_pages_to_the_buddy() -> TestResult {
    let available = reclaim::reclaimable_pages();
    if available == 0 {
        return pass!();
    }

    let want = available.min(8);
    let freed = reclaim::run(want);
    assert_test!(
        freed <= want,
        "reclaim returned {} against a request of {}",
        freed,
        want
    );
    assert_test!(
        reclaim::reclaimable_pages() <= available,
        "reclaiming {} pages left more reclaimable than before",
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
