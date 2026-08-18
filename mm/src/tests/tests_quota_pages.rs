//! The `Pages` axis: an address space's mapped pages, charged exactly. Unlike
//! every other kind, this charge tracks a quantity that *changes* over the
//! holder's life — a mapping split down the middle, two mappings merged into
//! one, a whole address space torn down — each a different way for the number
//! to drift from the tree it summarises.

use slopos_abi::quota::{QuotaMode, ResourceKind};
use slopos_abi::syscall::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE};
use slopos_ostd::klog_info;
use slopos_ostd::process::quota::{
    LedgerFault, quota_mode, set_limit, set_quota_mode, stats, try_charge,
};
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use super::tests::resolve_pid;
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::process_vm::{
    create_process_vm, destroy_process_vm, process_vm_mmap, process_vm_munmap,
};

const MAP_FLAGS: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const PROT_RW: u64 = PROT_READ | PROT_WRITE;

/// A scratch address space plus its account, torn down on drop: a row left
/// behind carrying a deliberately-tiny ceiling is indistinguishable, to the
/// headroom gate, from a real workload refused.
struct Scratch {
    pid: u32,
    restore: QuotaMode,
}

impl Scratch {
    fn new() -> Option<Self> {
        let restore = quota_mode();
        set_quota_mode(QuotaMode::Enforce);
        let pid = create_process_vm();
        if pid == slopos_abi::task::INVALID_PROCESS_ID {
            set_quota_mode(restore);
            return None;
        }
        Some(Self { pid, restore })
    }

    fn account(&self) -> slopos_ostd::process::AccountId {
        resolve_pid(self.pid).account()
    }

    fn used(&self) -> u32 {
        stats(self.account(), ResourceKind::Pages).map_or(0, |s| s.used)
    }

    fn mmap(&self, pages: u64) -> u64 {
        process_vm_mmap(
            resolve_pid(self.pid),
            0,
            pages * PAGE_SIZE_4KB,
            PROT_RW,
            MAP_FLAGS,
            -1,
            0,
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        destroy_process_vm(resolve_pid(self.pid));
        set_quota_mode(self.restore);
    }
}

/// An mmap charges exactly its page count, and munmap gives exactly it back.
pub fn test_quota_pages_mmap_charges_exactly() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    const PAGES: u64 = 16;

    let before = scratch.used();
    let addr = scratch.mmap(PAGES);
    assert_test!(addr != 0, "mmap failed");
    let charged = scratch.used() - before;
    assert_test!(
        charged == PAGES as u32,
        "mmap of {} pages charged {}",
        PAGES,
        charged
    );

    process_vm_munmap(resolve_pid(scratch.pid), addr, PAGES * PAGE_SIZE_4KB);
    let after = scratch.used();
    assert_test!(
        after == before,
        "munmap left {} pages charged, want the {} it started at",
        after,
        before
    );
    pass!()
}

/// The case a per-region scalar charge cannot express: the region splits into
/// two remnants, and the charge must fall by the carved pages and no more.
/// Either error is silent — too large a refund hands out headroom that does not
/// exist, too small pins it forever.
pub fn test_quota_pages_split_refunds_only_the_hole() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    const PAGES: u64 = 16;
    const HOLE_AT: u64 = 4;
    const HOLE_PAGES: u64 = 4;

    let before = scratch.used();
    let addr = scratch.mmap(PAGES);
    assert_test!(addr != 0, "mmap failed");

    process_vm_munmap(
        resolve_pid(scratch.pid),
        addr + HOLE_AT * PAGE_SIZE_4KB,
        HOLE_PAGES * PAGE_SIZE_4KB,
    );

    let held = scratch.used() - before;
    let want = (PAGES - HOLE_PAGES) as u32;
    assert_test!(
        held == want,
        "after carving {} pages from {}, the charge reads {} -- want {}",
        HOLE_PAGES,
        PAGES,
        held,
        want
    );

    process_vm_munmap(resolve_pid(scratch.pid), addr, PAGES * PAGE_SIZE_4KB);
    assert_test!(
        scratch.used() == before,
        "unmapping both remnants did not return the charge to baseline"
    );
    pass!()
}

/// The mirror of the split: `VmaMap::insert` absorbs an adjacent compatible
/// neighbour and widens the new entry by exactly as much, so a charge that
/// re-paid for the absorbed pages would drift up by one region per merge and
/// never come back down.
pub fn test_quota_pages_merge_does_not_double_charge() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    const PAGES: u64 = 8;

    let before = scratch.used();
    let first = scratch.mmap(PAGES);
    assert_test!(first != 0, "first mmap failed");
    let second = scratch.mmap(PAGES);
    assert_test!(second != 0, "second mmap failed");

    let held = scratch.used() - before;
    let want = (2 * PAGES) as u32;
    assert_test!(
        held == want,
        "two {}-page mappings charged {} -- want {} however they merged",
        PAGES,
        held,
        want
    );

    process_vm_munmap(resolve_pid(scratch.pid), first, PAGES * PAGE_SIZE_4KB);
    process_vm_munmap(resolve_pid(scratch.pid), second, PAGES * PAGE_SIZE_4KB);
    assert_test!(
        scratch.used() == before,
        "the merged mapping did not fully refund"
    );
    pass!()
}

/// The property the whole axis exists for, and the one a check-then-charge
/// split gets wrong: a refusal has to be the identity on the row, not a debit
/// that is corrected afterwards.
pub fn test_quota_pages_ceiling_refuses_and_leaves_no_debit() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    const HEADROOM: u32 = 8;

    let baseline = scratch.used();
    set_limit(scratch.account(), ResourceKind::Pages, baseline + HEADROOM);

    // Well past the headroom just granted.
    let refused = scratch.mmap((HEADROOM as u64) * 4);
    assert_test!(refused == 0, "an mmap over the page ceiling was granted");
    assert_test!(
        scratch.used() == baseline,
        "a refused mmap left {} pages charged, want {}",
        scratch.used(),
        baseline
    );

    let denials = stats(scratch.account(), ResourceKind::Pages).map_or(0, |s| s.denials);
    assert_test!(denials > 0, "a refusal nobody counted is a silent denial");

    // A grant under the ceiling is what separates the limit from a wedged
    // address space.
    let granted = scratch.mmap(HEADROOM as u64);
    assert_test!(granted != 0, "an mmap within the ceiling was refused");
    process_vm_munmap(
        resolve_pid(scratch.pid),
        granted,
        (HEADROOM as u64) * PAGE_SIZE_4KB,
    );
    pass!()
}

/// The leak this catches is unbounded across a boot: an address space whose
/// charge outlives it holds its principal's headroom forever, and because the
/// row is only reissued when the *slot* is reused, nothing else retires it.
pub fn test_quota_pages_teardown_returns_everything() -> TestResult {
    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    let pid = create_process_vm();
    if pid == slopos_abi::task::INVALID_PROCESS_ID {
        set_quota_mode(restore);
        return fail!("could not create an address space");
    }
    // Neither row can answer this on its own: teardown darkens the address
    // space's own row, which then reads zero whether or not the charge came
    // back, and the root is a shared ancestor moving with every other live
    // address space in the suite. What is invariant is the reconciliation —
    // after teardown no map exists, so no page may remain claimed against it.
    let account = resolve_pid(pid).account();

    let addr = process_vm_mmap(
        resolve_pid(pid),
        0,
        64 * PAGE_SIZE_4KB,
        PROT_RW,
        MAP_FLAGS,
        -1,
        0,
    );
    let mapped = stats(account, ResourceKind::Pages).map_or(0, |s| s.used);
    destroy_process_vm(resolve_pid(pid));

    let mut mismatches = 0usize;
    slopos_ostd::process::quota::ledger_audit(|fault| {
        if matches!(fault, LedgerFault::PagesMismatch { .. }) {
            mismatches += 1;
        }
    });
    set_quota_mode(restore);

    assert_test!(addr != 0, "mmap failed");
    assert_test!(
        mapped >= 64,
        "a 64-page mmap left the address space's row at {}",
        mapped
    );
    assert_test!(
        mismatches == 0,
        "after teardown {} address space(s) still claim pages they do not map",
        mismatches
    );
    pass!()
}

/// The drift is planted by charging the account behind the map's back — the
/// shape a mutation that forgot to adjust its charge would produce. `Pages` is
/// the one kind the other three checks cannot cover: they compare rows against
/// each other, and a charge that drifted from its *map* is consistent with
/// every ancestor.
pub fn test_quota_pages_audit_catches_a_drifted_charge() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    let addr = scratch.mmap(8);
    assert_test!(addr != 0, "mmap failed");

    let mut found = 0usize;
    slopos_ostd::process::quota::ledger_audit(|fault| {
        if matches!(fault, LedgerFault::PagesMismatch { .. }) {
            found += 1;
        }
    });
    assert_test!(found == 0, "a settled address space audited as mismatched");

    let planted =
        try_charge::<slopos_abi::quota::PagesAxis>(scratch.account(), 5).expect("planting a drift");

    let mut mismatch = None;
    slopos_ostd::process::quota::ledger_audit(|fault| {
        if mismatch.is_none()
            && let LedgerFault::PagesMismatch { .. } = fault
        {
            mismatch = Some(fault);
        }
    });
    drop(planted);

    let Some(LedgerFault::PagesMismatch {
        mapped,
        charged,
        used,
        ..
    }) = mismatch
    else {
        klog_info!("QUOTA_TEST: audit found no PagesMismatch for a planted drift");
        return fail!("the audit missed a drifted page charge");
    };
    assert_test!(
        mapped == charged,
        "the tokens stopped matching their maps ({} vs {}) -- the planted \
         drift was a row debit, so these two must still agree",
        mapped,
        charged
    );
    assert_test!(
        used == charged + 5,
        "the audit reported charged={} used={}, want a phantom debit of exactly 5",
        charged,
        used
    );

    process_vm_munmap(resolve_pid(scratch.pid), addr, 8 * PAGE_SIZE_4KB);
    pass!()
}

slopos_testing::stest!(
    name = test_quota_pages_mmap_charges_exactly,
    suite = quota_pages
);
slopos_testing::stest!(
    name = test_quota_pages_split_refunds_only_the_hole,
    suite = quota_pages
);
slopos_testing::stest!(
    name = test_quota_pages_merge_does_not_double_charge,
    suite = quota_pages
);
slopos_testing::stest!(
    name = test_quota_pages_ceiling_refuses_and_leaves_no_debit,
    suite = quota_pages
);
slopos_testing::stest!(
    name = test_quota_pages_teardown_returns_everything,
    suite = quota_pages
);
slopos_testing::stest!(
    name = test_quota_pages_audit_catches_a_drifted_charge,
    suite = quota_pages
);

/// A keepalive's pin charge is independent of the buffer it was taken from,
/// which is what lets it outlive its `PinnedUserBuffer` while a NIC TX DMA is
/// still in flight. A shared charge would be refunded at ring teardown while
/// the driver still held the pages — a memory-lock bypass at the DMA boundary.
#[cfg(feature = "test-hooks")]
pub fn test_quota_keepalive_charge_outlives_its_pin() -> TestResult {
    use crate::pinned_user_buffer::PinnedUserBuffer;

    let account = slopos_ostd::process::quota::root();
    let pinned = || stats(account, ResourceKind::PinnedBytes).map_or(0, |s| s.used);

    let before = pinned();
    let Some(pin) = PinnedUserBuffer::alloc_for_test(8192) else {
        return fail!("pin alloc_for_test failed");
    };
    let Some(keepalive) = pin.keepalive_frames(account) else {
        return fail!("keepalive_frames returned None");
    };
    let with_both = pinned();
    assert_test!(
        with_both >= before + 2,
        "a 2-page keepalive charged {} pages",
        with_both - before
    );

    drop(pin);
    let after_pin = pinned();
    assert_test!(
        after_pin >= before + 2,
        "dropping the pin refunded the keepalive's charge ({} -> {}) -- the \
         driver may still be DMAing those pages",
        with_both,
        after_pin
    );

    drop(keepalive);
    assert_test!(
        pinned() == before,
        "dropping the keepalive left {} pages charged, want {}",
        pinned(),
        before
    );
    pass!()
}

/// Two in-flight DMAs of the same pages hold them down twice over, so counting
/// them once would let a retransmit storm pin arbitrarily many pages against a
/// single charge. `redup` also re-uses the original's account rather than the
/// caller's, so a retransmit cannot re-home a pin onto whoever happens to be
/// running the send path.
#[cfg(feature = "test-hooks")]
pub fn test_quota_keepalive_redup_charges_each_dma() -> TestResult {
    use crate::pinned_user_buffer::PinnedUserBuffer;

    let account = slopos_ostd::process::quota::root();
    let pinned = || stats(account, ResourceKind::PinnedBytes).map_or(0, |s| s.used);

    let Some(pin) = PinnedUserBuffer::alloc_for_test(8192) else {
        return fail!("pin alloc_for_test failed");
    };
    let Some(first) = pin.keepalive_frames(account) else {
        return fail!("keepalive_frames returned None");
    };
    let one = pinned();
    let Some(second) = first.redup() else {
        return fail!("redup returned None");
    };
    let two = pinned();

    assert_test!(
        two == one + 2,
        "a retransmit's keepalive charged {} pages, want 2",
        two - one
    );
    drop(second);
    assert_test!(
        pinned() == one,
        "releasing one in-flight DMA did not refund exactly its own pin"
    );
    drop(first);
    drop(pin);
    pass!()
}

#[cfg(feature = "test-hooks")]
slopos_testing::stest!(
    name = test_quota_keepalive_charge_outlives_its_pin,
    suite = quota_pages
);
#[cfg(feature = "test-hooks")]
slopos_testing::stest!(
    name = test_quota_keepalive_redup_charges_each_dma,
    suite = quota_pages
);
