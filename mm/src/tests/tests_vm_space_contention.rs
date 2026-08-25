//! A second `KArc<VmSpace>` is minted only under the per-process lock and
//! released outside it, so holding one and then driving a fault path on the
//! same CPU reproduces the contention single-threaded — no SMP, no timing.

use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::frame::{AnonymousMeta, Frame, Paddr};

use crate::error::MmError;
use crate::page_fault::{
    FaultOutcome, RETRY_WARN_MS, RetryEpisode, note_retry, try_resolve_user_fault,
};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::{
    pack_process_vm_handle, process_vm_alloc, process_vm_brk, process_vm_get_vm_space,
    process_vm_handle,
};
use crate::tests::test_fixtures::ProcessVmGuard;

const WRITE_USER_ABSENT: u64 = 0x06;

fn lazy_anon_page(vm: &ProcessVmGuard) -> Option<u64> {
    let addr = process_vm_alloc(vm.process, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    if addr == 0 { None } else { Some(addr) }
}

fn cow_page(vm: &ProcessVmGuard) -> Option<PhysAddr> {
    let phys = vm.map_test_page(0x5000, PageFlags::USER_RO.bits())?;
    vm.mark_cow(0x5000);
    Some(phys)
}

pub fn test_demand_fault_retries_while_a_reader_holds_the_space() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };
    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };

    let result = vm.handle_demand_fault(addr, WRITE_USER_ABSENT);
    drop(reader);

    match result {
        Err(MmError::Retry) => pass!(),
        other => fail!("expected Retry, got {:?}", other),
    }
}

pub fn test_demand_fault_retry_maps_nothing() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };
    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };

    let result = vm.handle_demand_fault(addr, WRITE_USER_ABSENT);
    let mapped = vm.virt_to_phys(addr);
    drop(reader);

    assert_test!(result == Err(MmError::Retry), "expected Retry");
    assert_test!(mapped.is_null(), "a retried demand fault left a mapping");
    pass!()
}

pub fn test_demand_fault_resolves_after_the_reader_drops() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };
    let blocked = vm.handle_demand_fault(addr, WRITE_USER_ABSENT);
    drop(reader);
    assert_test!(blocked == Err(MmError::Retry), "expected Retry while held");

    if let Err(e) = vm.handle_demand_fault(addr, WRITE_USER_ABSENT) {
        return fail!("demand fault failed after the reader dropped: {:?}", e);
    }
    assert_test!(
        !vm.virt_to_phys(addr).is_null(),
        "demand fault reported success without a mapping"
    );
    pass!()
}

pub fn test_cow_fault_retries_while_a_reader_holds_the_space() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(phys) = cow_page(&vm) else {
        return fail!("map and mark a COW page");
    };
    // Equivalent to a second process mapping the same paddr, which is what
    // sends `handle_cow_fault` down the copying arm.
    let shared = match Frame::<AnonymousMeta>::from_in_use(Paddr::new(phys.as_u64())) {
        Ok(frame) => frame,
        Err(e) => return fail!("take a second reference to the COW frame: {:?}", e),
    };

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        drop(shared);
        return fail!("clone the address space");
    };
    let result = vm.handle_cow_fault(0x5000);
    drop(reader);
    drop(shared);

    match result {
        Err(MmError::Retry) => pass!(),
        other => fail!("expected Retry, got {:?}", other),
    }
}

pub fn test_cow_retry_leaves_the_page_mapped_and_cow() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(phys) = cow_page(&vm) else {
        return fail!("map and mark a COW page");
    };
    let shared = match Frame::<AnonymousMeta>::from_in_use(Paddr::new(phys.as_u64())) {
        Ok(frame) => frame,
        Err(e) => return fail!("take a second reference to the COW frame: {:?}", e),
    };

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        drop(shared);
        return fail!("clone the address space");
    };
    let result = vm.handle_cow_fault(0x5000);
    let after = vm.virt_to_phys(0x5000);
    let still_cow = vm.is_cow(0x5000);
    drop(reader);
    drop(shared);

    assert_test!(result == Err(MmError::Retry), "expected Retry");
    assert_test!(after == phys, "a retried COW fault replaced the leaf");
    assert_test!(still_cow, "a retried COW fault cleared the COW bit");
    pass!()
}

pub fn test_user_fault_dispatch_reports_retry_not_fatal() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };
    let Some(handle) = process_vm_handle(vm.process) else {
        return fail!("resolve the process-VM handle");
    };
    let packed = pack_process_vm_handle(handle);

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };
    let outcome = try_resolve_user_fault(addr, WRITE_USER_ABSENT, packed, vm.pid());
    let mapped = vm.virt_to_phys(addr);
    drop(reader);

    assert_test!(
        outcome == FaultOutcome::Retry,
        "a contended address space must not be a fatal user fault"
    );
    assert_test!(mapped.is_null(), "the retried fault left a mapping");
    pass!()
}

pub fn test_user_fault_dispatch_resolves_after_the_reader_drops() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };
    let Some(handle) = process_vm_handle(vm.process) else {
        return fail!("resolve the process-VM handle");
    };
    let packed = pack_process_vm_handle(handle);

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };
    let blocked = try_resolve_user_fault(addr, WRITE_USER_ABSENT, packed, vm.pid());
    drop(reader);
    assert_test!(blocked == FaultOutcome::Retry, "expected Retry while held");

    let outcome = try_resolve_user_fault(addr, WRITE_USER_ABSENT, packed, vm.pid());
    assert_test!(
        outcome == FaultOutcome::Resolved,
        "the fault did not resolve once the reader dropped"
    );
    pass!()
}

pub fn test_map_user_range_leaves_nothing_mapped_on_would_block() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let base = process_vm_brk(vm.process, 0);
    assert_test!(base != 0, "initial brk is 0");
    let target = base + 2 * PAGE_SIZE_4KB;

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };
    let grown = process_vm_brk(vm.process, target);
    let first = vm.virt_to_phys(base);
    let second = vm.virt_to_phys(base + PAGE_SIZE_4KB);
    drop(reader);

    assert_test!(grown == 0, "brk grew while a reader held the address space");
    assert_test!(
        first.is_null() && second.is_null(),
        "a failed multi-page map left part of the range mapped"
    );
    pass!()
}

pub fn test_retry_episode_warns_once_after_the_budget() -> TestResult {
    let mut ep = RetryEpisode::IDLE;
    assert_test!(
        !note_retry(&mut ep, 7, 0x1000, 0),
        "the first retry of an episode warned"
    );
    assert_test!(
        !note_retry(&mut ep, 7, 0x1000, RETRY_WARN_MS - 1),
        "warned before the budget elapsed"
    );
    assert_test!(
        note_retry(&mut ep, 7, 0x1000, RETRY_WARN_MS),
        "did not warn once the budget elapsed"
    );
    assert_test!(
        !note_retry(&mut ep, 7, 0x1000, RETRY_WARN_MS + 1),
        "warned twice in one episode"
    );
    pass!()
}

/// A restart is only observable by warning again, so each arm below steps past
/// the budget from the moment the episode would have restarted: the call that
/// changes address or task returns `false` either way.
pub fn test_retry_episode_keys_on_the_task_not_the_address() -> TestResult {
    let mut ep = RetryEpisode::IDLE;
    assert_test!(
        !note_retry(&mut ep, 7, 0x1000, 0),
        "the first retry of an episode warned"
    );
    assert_test!(
        note_retry(&mut ep, 7, 0x1000, RETRY_WARN_MS),
        "did not warn once the budget elapsed"
    );

    assert_test!(
        !note_retry(&mut ep, 7, 0x2000, 200),
        "a new fault address warned on its first retry"
    );
    assert_test!(
        !note_retry(&mut ep, 7, 0x2000, 200 + RETRY_WARN_MS),
        "a new fault address restarted the episode: one task warned twice"
    );

    assert_test!(
        !note_retry(&mut ep, 8, 0x2000, 300),
        "a new task warned on its first retry"
    );
    assert_test!(
        !note_retry(&mut ep, 8, 0x2000, 300 + RETRY_WARN_MS - 1),
        "the new task's budget was measured from the previous episode"
    );
    assert_test!(
        note_retry(&mut ep, 8, 0x2000, 300 + RETRY_WARN_MS),
        "a new task did not restart the episode: it never warned"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_demand_fault_retries_while_a_reader_holds_the_space,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_demand_fault_retry_maps_nothing,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_demand_fault_resolves_after_the_reader_drops,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_cow_fault_retries_while_a_reader_holds_the_space,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_cow_retry_leaves_the_page_mapped_and_cow,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_user_fault_dispatch_reports_retry_not_fatal,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_user_fault_dispatch_resolves_after_the_reader_drops,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_map_user_range_leaves_nothing_mapped_on_would_block,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_retry_episode_warns_once_after_the_budget,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_retry_episode_keys_on_the_task_not_the_address,
    suite = vm_contention
);
