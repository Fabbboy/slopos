use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::demand::{can_satisfy_fault, is_demand_fault};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::{process_vm_alloc, process_vm_get_region};
use crate::tests::test_fixtures::ProcessVmGuard;
use crate::vma_region::{Protection, RegionBacking, RegionPurpose, VmaRegion};

/// Helper to construct anonymous lazy user VMA regions for tests.
fn anon_region(prot: Protection) -> VmaRegion {
    VmaRegion {
        protection: prot,
        backing: RegionBacking::Anonymous,
        lazy: true,
        cow: false,
        user: true,
        purpose: RegionPurpose::General,
    }
}

fn kernel_region(prot: Protection) -> VmaRegion {
    VmaRegion {
        protection: prot,
        backing: RegionBacking::Anonymous,
        lazy: true,
        cow: false,
        user: false,
        purpose: RegionPurpose::General,
    }
}

pub fn test_demand_fault_present_page() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let Some(_phys) = vm.map_test_page(addr, PageFlags::USER_RW.bits()) else {
        return fail!("map test page");
    };

    let error_code_present: u64 = 0x01;
    assert_test!(
        !is_demand_fault(error_code_present, vm.pid, addr),
        "is_demand_fault returned true for present page"
    );

    pass!()
}

pub fn test_demand_fault_no_vma() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let unmapped_addr: u64 = 0x7FFF_0000_0000;
    let error_code: u64 = 0x06;

    assert_test!(
        !is_demand_fault(error_code, vm.pid, unmapped_addr),
        "is_demand_fault returned true for unmapped address"
    );

    pass!()
}

pub fn test_demand_fault_lazy_anon_vma() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let region = process_vm_get_region(vm.pid, addr);
    assert_test!(region.is_some(), "no VMA found for allocated address");
    let region = region.unwrap();
    assert_test!(region.is_demand_paged(), "allocated VMA is not LAZY");
    assert_test!(region.is_anonymous(), "allocated VMA is not Anonymous");

    let error_code: u64 = 0x04;
    assert_test!(
        is_demand_fault(error_code, vm.pid, addr),
        "is_demand_fault returned false for valid LAZY VMA"
    );

    pass!()
}

pub fn test_demand_permission_deny_write_ro() -> TestResult {
    let ro = anon_region(Protection::RO);
    let error_code_write: u64 = 0x06;

    assert_test!(
        !can_satisfy_fault(error_code_write, &ro),
        "can_satisfy_fault allowed write to read-only VMA"
    );

    pass!()
}

pub fn test_demand_permission_deny_user_kernel() -> TestResult {
    let kernel = kernel_region(Protection::RW);
    let error_code_user: u64 = 0x04;

    assert_test!(
        !can_satisfy_fault(error_code_user, &kernel),
        "can_satisfy_fault allowed user access to kernel VMA"
    );

    pass!()
}

pub fn test_demand_permission_deny_exec() -> TestResult {
    let data = anon_region(Protection::RW);
    let error_code_ifetch: u64 = 0x14;

    assert_test!(
        !can_satisfy_fault(error_code_ifetch, &data),
        "can_satisfy_fault allowed exec on non-exec VMA"
    );

    pass!()
}

pub fn test_demand_permission_allow_read() -> TestResult {
    let readable = anon_region(Protection::RO);
    let error_code_read: u64 = 0x04;

    assert_test!(
        can_satisfy_fault(error_code_read, &readable),
        "can_satisfy_fault denied valid read"
    );

    pass!()
}

pub fn test_demand_permission_allow_write() -> TestResult {
    let writable = anon_region(Protection::RW);
    let error_code_write: u64 = 0x06;

    assert_test!(
        can_satisfy_fault(error_code_write, &writable),
        "can_satisfy_fault denied valid write"
    );

    pass!()
}

pub fn test_demand_handle_null_page_dir() -> TestResult {
    // After the framekernel migration, `demand::handle_demand_fault`
    // is reached only via `process_vm::process_vm_with_dual_paging`,
    // which filters out slots with no address space. Verify the
    // dispatcher returns `None` for an unknown PID — the failure path
    // `try_resolve_user_fault` actually hits.
    let result = crate::process_vm::process_vm_with_dual_paging(
        slopos_abi::task::INVALID_PROCESS_ID,
        |_| (),
    );
    if result.is_some() {
        return fail!("process_vm_with_dual_paging returned Some for INVALID_PROCESS_ID");
    }
    pass!()
}
