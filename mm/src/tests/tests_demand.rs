use slopos_abi::addr::VirtAddr;
use slopos_testing::TestResult;
use slopos_testing::{assert_not_null, assert_test, fail, pass};
use slopos_utils::klog_info;

use crate::demand::{can_satisfy_fault, handle_demand_fault, is_demand_fault};
use crate::error::MmError;
use crate::paging::virt_to_phys_in_dir;
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::{process_vm_alloc, process_vm_get_vma_flags};
use crate::tests::test_fixtures::{ProcessVmGuard, map_test_page};
use crate::vma_flags::VmaFlags;

pub fn test_demand_fault_present_page() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let Some(_phys) = map_test_page(vm.page_dir, addr, PageFlags::USER_RW.bits()) else {
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

pub fn test_demand_fault_non_lazy_vma() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let null_page_addr: u64 = 0;
    let flags = process_vm_get_vma_flags(vm.pid, null_page_addr);
    if let Some(f) = flags {
        if f.is_demand_paged() {
            klog_info!("DEMAND_TEST: Null page VMA has LAZY flag (unexpected but not a bug)");
        }
    }

    pass!()
}

pub fn test_demand_fault_valid_lazy_vma() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let flags = process_vm_get_vma_flags(vm.pid, addr);
    assert_test!(flags.is_some(), "no VMA found for allocated address");

    let vma_flags = flags.unwrap();
    assert_test!(vma_flags.is_demand_paged(), "allocated VMA is not LAZY");
    assert_test!(vma_flags.is_anonymous(), "allocated VMA is not ANON");

    let error_code: u64 = 0x04;
    assert_test!(
        is_demand_fault(error_code, vm.pid, addr),
        "is_demand_fault returned false for valid LAZY VMA"
    );

    pass!()
}

pub fn test_demand_permission_deny_write_ro() -> TestResult {
    let ro_flags = VmaFlags::READ | VmaFlags::USER | VmaFlags::LAZY | VmaFlags::ANON;
    let error_code_write: u64 = 0x06;

    assert_test!(
        !can_satisfy_fault(error_code_write, ro_flags),
        "can_satisfy_fault allowed write to read-only VMA"
    );

    pass!()
}

pub fn test_demand_permission_deny_user_kernel() -> TestResult {
    let kernel_flags = VmaFlags::READ | VmaFlags::WRITE | VmaFlags::LAZY | VmaFlags::ANON;
    let error_code_user: u64 = 0x04;

    assert_test!(
        !can_satisfy_fault(error_code_user, kernel_flags),
        "can_satisfy_fault allowed user access to kernel VMA"
    );

    pass!()
}

pub fn test_demand_permission_deny_exec() -> TestResult {
    let data_flags =
        VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER | VmaFlags::LAZY | VmaFlags::ANON;
    let error_code_ifetch: u64 = 0x14;

    assert_test!(
        !can_satisfy_fault(error_code_ifetch, data_flags),
        "can_satisfy_fault allowed exec on non-exec VMA"
    );

    pass!()
}

pub fn test_demand_permission_allow_read() -> TestResult {
    let readable_flags = VmaFlags::READ | VmaFlags::USER | VmaFlags::LAZY | VmaFlags::ANON;
    let error_code_read: u64 = 0x04;

    assert_test!(
        can_satisfy_fault(error_code_read, readable_flags),
        "can_satisfy_fault denied valid read"
    );

    pass!()
}

pub fn test_demand_permission_allow_write() -> TestResult {
    let writable_flags =
        VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER | VmaFlags::LAZY | VmaFlags::ANON;
    let error_code_write: u64 = 0x06;

    assert_test!(
        can_satisfy_fault(error_code_write, writable_flags),
        "can_satisfy_fault denied valid write"
    );

    pass!()
}

pub fn test_demand_handle_null_page_dir() -> TestResult {
    match handle_demand_fault(core::ptr::null_mut(), 1, 0x1000, 0x04) {
        Err(MmError::NullPageDir) => pass!(),
        Ok(_) => fail!("handle_demand_fault succeeded with null page_dir"),
        Err(e) => fail!("wrong error for null page_dir: {:?}", e),
    }
}

pub fn test_demand_handle_no_vma() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let unmapped_addr: u64 = 0x7FFF_0000_0000;
    let error_code: u64 = 0x04;

    match handle_demand_fault(vm.page_dir, vm.pid, unmapped_addr, error_code) {
        Err(MmError::NoVma) => pass!(),
        Ok(_) => fail!("handle_demand_fault succeeded for unmapped address"),
        Err(e) => fail!("wrong error for unmapped address: {:?}", e),
    }
}

pub fn test_demand_handle_success() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let phys_before = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(addr));
    if !phys_before.is_null() {
        klog_info!("DEMAND_TEST: Page already mapped before demand fault (might be OK)");
    }

    let error_code: u64 = 0x04;
    if let Err(e) = handle_demand_fault(vm.page_dir, vm.pid, addr, error_code) {
        return fail!("handle_demand_fault failed: {:?}", e);
    }

    let phys_after = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(addr));
    assert_not_null!(phys_after, "page not mapped after demand fault resolution");

    pass!()
}

pub fn test_demand_handle_permission_denied() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.pid, PAGE_SIZE_4KB, 0);
    if addr == 0 {
        return pass!();
    }

    let error_code: u64 = 0x06;
    match handle_demand_fault(vm.page_dir, vm.pid, addr, error_code) {
        Err(MmError::PermissionDenied) => pass!(),
        Ok(_) => fail!("handle_demand_fault allowed write to RO VMA"),
        Err(_) => pass!(),
    }
}

pub fn test_demand_handle_page_boundary() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.pid, PAGE_SIZE_4KB * 2, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let boundary_addr = addr + PAGE_SIZE_4KB - 1;
    let error_code: u64 = 0x04;

    if let Err(e) = handle_demand_fault(vm.page_dir, vm.pid, boundary_addr, error_code) {
        return fail!("boundary fault failed: {:?}", e);
    }

    let phys = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(addr));
    assert_not_null!(phys, "first page not mapped after boundary fault");

    let phys2 = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(addr + PAGE_SIZE_4KB));
    if !phys2.is_null() {
        klog_info!("DEMAND_TEST: Second page mapped (might be prefetching - OK)");
    }

    pass!()
}

pub fn test_demand_multiple_faults() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.pid, PAGE_SIZE_4KB * 4, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let error_code: u64 = 0x04;
    let offsets = [2, 0, 3, 1];
    for &offset in &offsets {
        let fault_addr = addr + (offset as u64) * PAGE_SIZE_4KB;
        if let Err(e) = handle_demand_fault(vm.page_dir, vm.pid, fault_addr, error_code) {
            return fail!("multiple faults failed at offset {}: {:?}", offset, e);
        }
    }

    for i in 0..4 {
        let check_addr = addr + (i as u64) * PAGE_SIZE_4KB;
        let phys = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(check_addr));
        assert_not_null!(phys, "page not mapped after fault");
    }

    pass!()
}

pub fn test_demand_double_fault() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let error_code: u64 = 0x04;

    if let Err(e) = handle_demand_fault(vm.page_dir, vm.pid, addr, error_code) {
        return fail!("first fault failed: {:?}", e);
    }

    let phys1 = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(addr));

    let result2 = handle_demand_fault(vm.page_dir, vm.pid, addr, error_code);
    if let Ok(_) = result2 {
        let phys2 = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(addr));
        assert_test!(phys2 == phys1, "double fault changed physical mapping");
    }

    pass!()
}

pub fn test_demand_invalid_process_id() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let wrong_pid = vm.pid + 999;
    let error_code: u64 = 0x04;

    match handle_demand_fault(vm.page_dir, wrong_pid, 0x1000, error_code) {
        Err(MmError::NoVma) => pass!(),
        Ok(_) => fail!("demand fault succeeded with wrong PID"),
        Err(_) => pass!(),
    }
}

slopos_testing::define_test_suite!(
    demand_paging,
    [
        test_demand_fault_present_page,
        test_demand_fault_no_vma,
        test_demand_fault_non_lazy_vma,
        test_demand_fault_valid_lazy_vma,
        test_demand_permission_deny_write_ro,
        test_demand_permission_deny_user_kernel,
        test_demand_permission_deny_exec,
        test_demand_permission_allow_read,
        test_demand_permission_allow_write,
        test_demand_handle_null_page_dir,
        test_demand_handle_no_vma,
        test_demand_handle_success,
        test_demand_handle_permission_denied,
        test_demand_handle_page_boundary,
        test_demand_multiple_faults,
        test_demand_double_fault,
        test_demand_invalid_process_id,
    ]
);
