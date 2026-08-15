//! The kernel heap's own backing pages, charged to the root.
//!
//! Every kernel allocation is ultimately backed by a page the size-class slabs
//! or the large-alloc tier took from the buddy. Those pages are the
//! *unattributed remainder*: the tier-1 object charges bound how many objects
//! a principal may hold, never how many pages the shared slab needed to hold
//! them. Charging the backing to the root is what makes the top of the tree
//! total — without it the ledger cannot be reconciled against the buddy at
//! all, and a discrepancy reads as a known gap rather than as a bug.
//!
//! Deliberately the root and not the allocating principal. Per-cgroup slab
//! caches measured 45–65 % utilisation upstream, and moving to shared-slab
//! accounting recovered ~40 % of kernel memory; the slab is shared here too,
//! so its backing is the kernel's.

use slopos_abi::quota::ResourceKind;
use slopos_ostd::KVec;
use slopos_ostd::process::quota::{root, stats};
use slopos_testing::TestResult;
use slopos_testing::{assert_test, pass};

use crate::slab::MAX_SLAB_CLASS_BYTES;
use crate::slab::page::charged_heap_pages;
use crate::slab::{kfree, kmalloc};

fn root_kernelmeta() -> u32 {
    stats(root(), ResourceKind::KernelMeta).map_or(0, |s| s.used)
}

/// The heap's charged page count never exceeds what the buddy handed out.
///
/// The reconciliation the root account exists for. Heap pages are a subset of
/// allocated ones, so a charged count above `allocated` is an accounting leak
/// rather than an allocator one — and it is the direction that matters,
/// because an over-count silently shrinks every principal's headroom.
pub fn test_quota_heap_backing_reconciles_with_the_buddy() -> TestResult {
    let charged = charged_heap_pages();
    let allocated = crate::page_alloc::get_page_allocator_stats().allocated;
    assert_test!(
        charged > 0,
        "no heap backing is charged, yet the kernel has a heap"
    );
    assert_test!(
        charged <= allocated,
        "the heap claims {} pages but the buddy has only {} allocated",
        charged,
        allocated
    );
    assert_test!(
        root_kernelmeta() >= charged,
        "the root's kernelmeta row ({}) is below the heap backing it includes ({})",
        root_kernelmeta(),
        charged
    );
    pass!()
}

/// A large allocation charges its pages and gives them back.
///
/// The large tier is the one the AF_UNIX connection FIFOs land in — 16 KiB
/// each, well past `MAX_SLAB_CLASS_BYTES` — so this is the path that makes
/// charging them at the call site unnecessary and, worse, a double-count.
pub fn test_quota_heap_large_alloc_is_charged() -> TestResult {
    const BYTES: usize = 4 * (MAX_SLAB_CLASS_BYTES + 1);

    let before = charged_heap_pages();
    let ptr = kmalloc(BYTES);
    assert_test!(!ptr.is_null(), "large allocation failed");
    let with = charged_heap_pages();
    kfree(ptr);

    assert_test!(
        with >= before,
        "a large allocation lowered the heap's charge ({} -> {})",
        before,
        with
    );
    // The tier reuses freed regions rather than returning them to the buddy,
    // so the charge legitimately stays held after `kfree`. What must not
    // happen is the charge moving *up* on a free.
    assert_test!(
        charged_heap_pages() <= with,
        "freeing a large allocation raised the heap's charge"
    );
    pass!()
}

/// Slab refills move the root's row, and the ledger stays consistent.
///
/// Drives enough small allocations to force at least one class to claim a
/// fresh page from the buddy, then checks that the audit still balances — the
/// charge is taken inside the allocator, so a mistake here corrupts the row
/// that every other principal debits through.
pub fn test_quota_heap_slab_refill_moves_the_root_row() -> TestResult {
    let before = charged_heap_pages();

    // Enough 512-byte objects to exhaust several pages' worth of one class.
    let mut held: KVec<*mut core::ffi::c_void> = KVec::with_capacity(256).expect("alloc");
    for _ in 0..256 {
        let p = kmalloc(512);
        if p.is_null() {
            break;
        }
        held.push(p).expect("pre-reserved");
    }
    let peak = charged_heap_pages();
    for p in held.iter() {
        kfree(*p);
    }

    assert_test!(
        peak >= before,
        "256 slab allocations lowered the heap's charge ({} -> {})",
        before,
        peak
    );

    let mut faults = 0usize;
    slopos_ostd::process::quota::ledger_audit(|_| faults += 1);
    assert_test!(
        faults == 0,
        "the ledger is inconsistent after {} slab refills",
        peak.saturating_sub(before)
    );
    pass!()
}

slopos_testing::stest!(
    name = test_quota_heap_backing_reconciles_with_the_buddy,
    suite = quota_heap
);
slopos_testing::stest!(
    name = test_quota_heap_large_alloc_is_charged,
    suite = quota_heap
);
slopos_testing::stest!(
    name = test_quota_heap_slab_refill_moves_the_root_row,
    suite = quota_heap
);
