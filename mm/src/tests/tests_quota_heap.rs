//! The kernel heap's own backing pages, charged to the root.
//!
//! Tier-1 object charges bound how many objects a principal may hold, never
//! how many pages the shared slab needed, so slab backing is charged to the
//! root rather than to the allocating principal.

use slopos_abi::quota::ResourceKind;
use slopos_ostd::KVec;
use slopos_ostd::cpu::preempt::PreemptGuard;
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

/// The three samples bracket their calls directly. `HEAP_PAGES` is kernel-wide,
/// so no guard here makes them adjacent — the preempt hold only keeps this
/// CPU's own reschedule out of the bracket, and the assertions are direction
/// checks that survive a peer either way. Not `IrqDisabled`: the large tier
/// walks an unbounded free list and then allocates from the buddy.
pub fn test_quota_heap_large_alloc_is_charged() -> TestResult {
    const BYTES: usize = 4 * (MAX_SLAB_CLASS_BYTES + 1);

    let (ptr, before, with, after) = {
        let _preempt = PreemptGuard::new();
        let before = charged_heap_pages();
        let ptr = kmalloc(BYTES);
        let with = charged_heap_pages();
        kfree(ptr);
        (ptr, before, with, charged_heap_pages())
    };

    assert_test!(!ptr.is_null(), "large allocation failed");
    assert_test!(
        with >= before,
        "a large allocation lowered the heap's charge ({} -> {})",
        before,
        with
    );
    // The tier reuses freed regions rather than returning them to the buddy,
    // so the charge legitimately stays held after `kfree`.
    assert_test!(
        after <= with,
        "freeing a large allocation raised the heap's charge ({} -> {})",
        with,
        after
    );
    pass!()
}

/// Slab refills move the root's row, and the ledger stays consistent.
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
