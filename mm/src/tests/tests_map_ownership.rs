//! A refused map must leave the page in exactly one owner's hands.
//!
//! The defect these cover freed the page twice: `ostd_map_4kb_user` took a
//! `PhysAddr`, minted the owning `UFrame` internally, and had no way to give it
//! back, so a refusal after the mint dropped it — freeing the page — while the
//! caller's error path freed the same paddr again.
//!
//! Neither free was observable from the caller: `free_phys` returns 0 both for
//! a real release and for a page whose descriptor is already `PCP`/`FREE`/
//! `QUIESCE`, which is what a second free finds. So these assert against the
//! `MetaSlot` refcount and the buddy's own accounting instead.

use slopos_abi::addr::VirtAddr;
use slopos_ostd::mm::frame::{AnonymousMeta, Paddr, reference_count_at};
use slopos_ostd::mm::page_table::fail_next_intermediate_allocs;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::MapError;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::page_alloc::{FrameAccounting, alloc_kernel_page, frame_accounting, free_page_frame};
use crate::paging_defs::PageFlags;
use crate::process_vm::process_vm_with_vm_space;
use crate::tests::test_fixtures::ProcessVmGuard;
use crate::user_mappings::{ostd_map_4kb_user, ostd_map_4kb_user_shared};

/// A VA in a fresh address space whose PDPT/PD/PT are all absent, so the
/// Create-mode walk must allocate an intermediate to reach the leaf — the
/// injector's target.
const UNPOPULATED_VA: u64 = 0x0000_0055_4000_0000;

/// The refused frame comes back to the caller, and dropping it is the page's
/// one and only release.
pub fn test_refused_map_returns_the_frame_once() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let pa = alloc_kernel_page();
    if pa.is_null() {
        return fail!("alloc page");
    }
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(Paddr::new(pa.as_u64())) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(pa);
            return fail!("claim: {:?}", e);
        }
    };
    assert_test!(reference_count_at(pa) == 1, "claimed frame holds one ref");

    // Every level below the PML4 is missing here, so the walk allocates.
    fail_next_intermediate_allocs(1);
    let outcome = process_vm_with_vm_space(vm.process, |vs| {
        ostd_map_4kb_user(
            vs,
            VirtAddr::new(UNPOPULATED_VA),
            frame,
            PageFlags::USER_RW.bits(),
        )
    });
    fail_next_intermediate_allocs(0);

    let returned = match outcome {
        Some(Err((returned, MapError::IntermediateAllocFailed))) => returned,
        Some(Err((_, e))) => return fail!("expected IntermediateAllocFailed, got {:?}", e),
        Some(Ok(())) => return fail!("the map succeeded despite the injected failure"),
        None => return fail!("no address space"),
    };

    assert_test!(
        returned.paddr().as_u64() == pa.as_u64(),
        "a different frame came back: {:#x} vs {:#x}",
        returned.paddr().as_u64(),
        pa.as_u64()
    );
    assert_test!(
        reference_count_at(pa) == 1,
        "the returned frame should still hold its single ref"
    );
    // A live owning ref over a free-listed page is the double-free's
    // signature, and the only assertion here that catches it: the refcount
    // alone reads identically either way.
    let accounting_while_held = frame_accounting(pa);
    assert_test!(
        accounting_while_held == FrameAccounting::HandedOut,
        "the refused page is {:?} while its owner still holds it -- it was \
         freed by the refusal",
        accounting_while_held
    );

    drop(returned);
    assert_test!(
        reference_count_at(pa) == 0,
        "dropping the returned frame did not release the page"
    );
    assert_test!(
        frame_accounting(pa) != FrameAccounting::HandedOut,
        "the page is still handed out after its owner dropped"
    );
    pass!()
}

/// The refusal must not install a leaf, so a caller that retries finds the VA
/// as it left it.
pub fn test_refused_map_installs_no_leaf() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let va = UNPOPULATED_VA + 0x1000;

    let pa = alloc_kernel_page();
    if pa.is_null() {
        return fail!("alloc page");
    }
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(Paddr::new(pa.as_u64())) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(pa);
            return fail!("claim: {:?}", e);
        }
    };

    fail_next_intermediate_allocs(1);
    let outcome = process_vm_with_vm_space(vm.process, |vs| {
        ostd_map_4kb_user(vs, VirtAddr::new(va), frame, PageFlags::USER_RW.bits())
    });
    fail_next_intermediate_allocs(0);

    let Some(Err((returned, _))) = outcome else {
        return fail!("expected a refusal");
    };
    let accounting_while_held = frame_accounting(pa);
    drop(returned);

    assert_test!(
        accounting_while_held == FrameAccounting::HandedOut,
        "the refused page is {:?} while its owner still holds it",
        accounting_while_held
    );
    assert_test!(
        vm.virt_to_phys(va).is_null(),
        "the refused map left a leaf behind"
    );
    pass!()
}

/// A refused *shared* map drops only the alias it added. The page belongs to
/// its origin — a memfd registry entry, or the parent's PTE across fork — and
/// must survive.
pub fn test_refused_shared_map_leaves_the_origin_holding() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(origin) = vm.map_test_page(0x9000, PageFlags::USER_RW.bits()) else {
        return fail!("map the origin page");
    };
    let held_before = reference_count_at(origin);
    assert_test!(
        held_before == 1,
        "the origin leaf should hold one ref, holds {}",
        held_before
    );

    let va = UNPOPULATED_VA + 0x2000;
    fail_next_intermediate_allocs(1);
    let outcome = process_vm_with_vm_space(vm.process, |vs| {
        ostd_map_4kb_user_shared(vs, VirtAddr::new(va), origin, PageFlags::USER_RW.bits())
    });
    fail_next_intermediate_allocs(0);

    assert_test!(
        matches!(outcome, Some(Err(MapError::IntermediateAllocFailed))),
        "expected IntermediateAllocFailed, got {:?}",
        outcome
    );
    assert_test!(
        reference_count_at(origin) == held_before,
        "the refused alias did not come back: {} refs, expected {}",
        reference_count_at(origin),
        held_before
    );
    assert_test!(
        frame_accounting(origin) == FrameAccounting::HandedOut,
        "the origin page was freed by a failed alias"
    );
    assert_test!(
        !vm.virt_to_phys(0x9000).is_null(),
        "the origin mapping was lost"
    );
    pass!()
}

/// The injector itself: if it stopped firing, every test above would pass by
/// mapping successfully and asserting nothing.
pub fn test_intermediate_alloc_injector_fires_once() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let va = UNPOPULATED_VA + 0x3000;

    let pa = alloc_kernel_page();
    if pa.is_null() {
        return fail!("alloc page");
    }
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(Paddr::new(pa.as_u64())) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(pa);
            return fail!("claim: {:?}", e);
        }
    };

    fail_next_intermediate_allocs(1);
    let first = process_vm_with_vm_space(vm.process, |vs| {
        ostd_map_4kb_user(vs, VirtAddr::new(va), frame, PageFlags::USER_RW.bits())
    });
    let Some(Err((frame, _))) = first else {
        return fail!("the injector did not fire on the first attempt");
    };

    // The count is spent, so the retry walks and maps for real.
    let second = process_vm_with_vm_space(vm.process, |vs| {
        ostd_map_4kb_user(vs, VirtAddr::new(va), frame, PageFlags::USER_RW.bits())
    });
    assert_test!(
        matches!(second, Some(Ok(()))),
        "the retry failed, so the injector is still armed"
    );
    assert_test!(
        vm.virt_to_phys(va).as_u64() == pa.as_u64(),
        "the retry mapped a different page"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_refused_map_returns_the_frame_once,
    suite = map_ownership
);
slopos_testing::stest!(
    name = test_refused_map_installs_no_leaf,
    suite = map_ownership
);
slopos_testing::stest!(
    name = test_refused_shared_map_leaves_the_origin_holding,
    suite = map_ownership
);
slopos_testing::stest!(
    name = test_intermediate_alloc_injector_fires_once,
    suite = map_ownership
);
