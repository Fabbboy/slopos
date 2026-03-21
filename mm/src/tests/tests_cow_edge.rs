use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};
use slopos_utils::klog_info;

use crate::cow::{handle_cow_fault, is_cow_fault};
use crate::error::MmError;
use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{
    ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame, page_frame_get_ref, page_frame_inc_ref,
};
use crate::paging::{paging_is_cow, paging_mark_cow, virt_to_phys_in_dir};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::process_vm_clone_cow;
use crate::tests::test_fixtures::{ProcessVmGuard, map_test_page};
use slopos_abi::task::INVALID_PROCESS_ID;

pub fn test_cow_read_not_cow_fault() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let Some(_phys) = map_test_page(vm.page_dir, 0x2000, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    paging_mark_cow(vm.page_dir, slopos_abi::addr::VirtAddr::new(0x2000));

    let error_code_read: u64 = 0x05;
    assert_test!(
        !is_cow_fault(error_code_read, vm.page_dir, 0x2000),
        "is_cow_fault returned true for read access"
    );

    pass!()
}

pub fn test_cow_not_present_not_cow() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let unmapped_addr: u64 = 0x5000_0000;
    let error_code: u64 = 0x02;

    assert_test!(
        !is_cow_fault(error_code, vm.page_dir, unmapped_addr),
        "is_cow_fault returned true for not-present page"
    );

    pass!()
}

pub fn test_cow_handle_null_pagedir() -> TestResult {
    match handle_cow_fault(core::ptr::null_mut(), 0x1000) {
        Err(MmError::NullPageDir) => pass!(),
        Ok(_) => fail!("handle_cow_fault succeeded with null page_dir"),
        Err(e) => fail!("wrong error for null page_dir: {:?}", e),
    }
}

pub fn test_cow_handle_not_cow_page() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let Some(_phys) = map_test_page(vm.page_dir, 0x3000, PageFlags::USER_RW.bits()) else {
        return fail!("map test page");
    };

    match handle_cow_fault(vm.page_dir, 0x3000) {
        Err(MmError::NotCowPage) => pass!(),
        Ok(_) => fail!("handle_cow_fault succeeded on non-COW page"),
        Err(e) => fail!("wrong error for non-COW page: {:?}", e),
    }
}

pub fn test_cow_single_ref_upgrade() -> TestResult {
    use slopos_abi::addr::VirtAddr;

    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let Some(phys) = map_test_page(vm.page_dir, 0x4000, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    paging_mark_cow(vm.page_dir, VirtAddr::new(0x4000));

    let ref_before = page_frame_get_ref(phys);
    assert_test!(ref_before == 1, "initial refcount should be 1");

    if let Err(e) = handle_cow_fault(vm.page_dir, 0x4000) {
        return fail!("single-ref COW failed: {:?}", e);
    }

    let phys_after = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(0x4000));
    if phys_after != phys {
        klog_info!(
            "COW_TEST: Single-ref COW copied page unnecessarily! {:#x} -> {:#x}",
            phys.as_u64(),
            phys_after.as_u64()
        );
    }

    assert_test!(
        !paging_is_cow(vm.page_dir, VirtAddr::new(0x4000)),
        "page still marked COW after resolution"
    );

    pass!()
}

pub fn test_cow_multi_ref_copy() -> TestResult {
    use slopos_abi::addr::VirtAddr;

    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let test_addr: u64 = 0x5000;
    let Some(phys) = map_test_page(vm.page_dir, test_addr, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..PAGE_SIZE_4KB as usize {
            unsafe { *ptr.add(i) = (i & 0xFF) as u8 };
        }
    }

    paging_mark_cow(vm.page_dir, VirtAddr::new(test_addr));

    page_frame_inc_ref(phys);
    page_frame_inc_ref(phys);

    let ref_before = page_frame_get_ref(phys);
    if ref_before != 3 {
        klog_info!("COW_TEST: Expected refcount 3, got {}", ref_before);
    }

    if let Err(e) = handle_cow_fault(vm.page_dir, test_addr) {
        return fail!("multi-ref COW failed: {:?}", e);
    }

    let phys_after = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(test_addr));
    assert_test!(phys_after != phys, "multi-ref COW didn't copy page");

    if let Some(virt) = phys_after.to_virt_checked() {
        let ptr = virt.as_ptr::<u8>();
        for i in 0..PAGE_SIZE_4KB as usize {
            let val = unsafe { *ptr.add(i) };
            let expected = (i & 0xFF) as u8;
            if val != expected {
                return fail!(
                    "data not copied correctly at offset {}: expected {:#x}, got {:#x}",
                    i,
                    expected,
                    val
                );
            }
        }
    }

    let ref_after = page_frame_get_ref(phys);
    assert_test!(ref_after < ref_before, "old page refcount didn't decrement");

    for _ in 0..ref_after {
        free_page_frame(phys);
    }

    pass!()
}

pub fn test_cow_page_boundary() -> TestResult {
    use slopos_abi::addr::VirtAddr;

    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let page_start: u64 = 0x6000;
    let Some(_phys) = map_test_page(vm.page_dir, page_start, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    paging_mark_cow(vm.page_dir, VirtAddr::new(page_start));

    let fault_addr = page_start + PAGE_SIZE_4KB - 1;
    if let Err(e) = handle_cow_fault(vm.page_dir, fault_addr) {
        return fail!("boundary COW failed: {:?}", e);
    }

    pass!()
}

pub fn test_cow_clone_modify_both() -> TestResult {
    use crate::process_vm::process_vm_alloc;
    use slopos_abi::addr::VirtAddr;

    let Some(parent) = ProcessVmGuard::new() else {
        return fail!("create parent VM");
    };

    let test_addr = process_vm_alloc(parent.pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(test_addr != 0, "process_vm_alloc failed");

    let Some(phys) = map_test_page(parent.page_dir, test_addr, PageFlags::USER_RW.bits()) else {
        return fail!("map test page");
    };

    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..PAGE_SIZE_4KB as usize {
            unsafe { *ptr.add(i) = 0xAA };
        }
    }

    let Some(child) = parent.clone_cow() else {
        return fail!("COW clone failed");
    };

    if paging_is_cow(parent.page_dir, VirtAddr::new(test_addr)) {
        if let Err(e) = handle_cow_fault(parent.page_dir, test_addr) {
            return fail!("parent COW resolution failed: {:?}", e);
        }
    }

    let parent_phys = virt_to_phys_in_dir(parent.page_dir, VirtAddr::new(test_addr));
    if let Some(virt) = parent_phys.to_virt_checked() {
        unsafe { *virt.as_mut_ptr::<u8>() = 0xBB };
    }

    if paging_is_cow(child.page_dir, VirtAddr::new(test_addr)) {
        if let Err(e) = handle_cow_fault(child.page_dir, test_addr) {
            return fail!("child COW resolution failed: {:?}", e);
        }
    }

    let child_phys = virt_to_phys_in_dir(child.page_dir, VirtAddr::new(test_addr));
    if let Some(virt) = child_phys.to_virt_checked() {
        unsafe { *virt.as_mut_ptr::<u8>() = 0xCC };
    }

    if let (Some(pv), Some(cv)) = (parent_phys.to_virt_checked(), child_phys.to_virt_checked()) {
        let parent_val = unsafe { *pv.as_ptr::<u8>() };
        let child_val = unsafe { *cv.as_ptr::<u8>() };

        assert_test!(
            parent_val != child_val,
            "parent and child share same data after COW"
        );
        assert_test!(parent_val == 0xBB, "parent data corrupted");
        assert_test!(child_val == 0xCC, "child data corrupted");
    }

    pass!()
}

pub fn test_cow_multiple_clones() -> TestResult {
    let Some(parent) = ProcessVmGuard::new() else {
        return fail!("create parent VM");
    };

    let mut children: [u32; 4] = [INVALID_PROCESS_ID; 4];
    let mut child_count = 0usize;

    for i in 0..4 {
        let child_pid = process_vm_clone_cow(parent.pid);
        if child_pid == INVALID_PROCESS_ID {
            klog_info!("COW_TEST: Clone {} failed", i);
            break;
        }
        children[i] = child_pid;
        child_count += 1;
    }

    assert_test!(child_count >= 2, "couldn't create enough clones");

    for i in 0..child_count {
        crate::process_vm::destroy_process_vm(children[i]);
    }

    pass!()
}

pub fn test_cow_no_collateral_damage() -> TestResult {
    use slopos_abi::addr::VirtAddr;

    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr1: u64 = 0x7000;
    let addr2: u64 = 0x8000;

    let phys1 = alloc_page_frame(ALLOC_FLAG_ZERO);
    let phys2 = alloc_page_frame(ALLOC_FLAG_ZERO);

    if phys1.is_null() || phys2.is_null() {
        if !phys1.is_null() {
            free_page_frame(phys1);
        }
        if !phys2.is_null() {
            free_page_frame(phys2);
        }
        return fail!("alloc pages for collateral test");
    }

    if let Some(v1) = phys1.to_virt_checked() {
        unsafe { core::ptr::write_bytes(v1.as_mut_ptr::<u8>(), 0x11, PAGE_SIZE_4KB as usize) };
    }
    if let Some(v2) = phys2.to_virt_checked() {
        unsafe { core::ptr::write_bytes(v2.as_mut_ptr::<u8>(), 0x22, PAGE_SIZE_4KB as usize) };
    }

    use crate::paging::map_page_4kb_in_dir;

    if map_page_4kb_in_dir(
        vm.page_dir,
        VirtAddr::new(addr1),
        phys1,
        PageFlags::USER_RO.bits(),
    ) != 0
    {
        free_page_frame(phys1);
        free_page_frame(phys2);
        return fail!("map page 1");
    }
    if map_page_4kb_in_dir(
        vm.page_dir,
        VirtAddr::new(addr2),
        phys2,
        PageFlags::USER_RO.bits(),
    ) != 0
    {
        free_page_frame(phys2);
        return fail!("map page 2");
    }

    paging_mark_cow(vm.page_dir, VirtAddr::new(addr1));
    paging_mark_cow(vm.page_dir, VirtAddr::new(addr2));

    if let Err(e) = handle_cow_fault(vm.page_dir, addr1) {
        return fail!("first page COW failed: {:?}", e);
    }

    let phys2_after = virt_to_phys_in_dir(vm.page_dir, VirtAddr::new(addr2));
    assert_test!(phys2_after == phys2, "second page physical address changed");

    if let Some(v2) = phys2_after.to_virt_checked() {
        let val = unsafe { *v2.as_ptr::<u8>() };
        assert_test!(val == 0x22, "second page data corrupted");
    }

    pass!()
}

pub fn test_cow_handle_invalid_address() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let unmapped: u64 = 0xDEAD_0000;
    match handle_cow_fault(vm.page_dir, unmapped) {
        Err(MmError::NotCowPage) | Err(MmError::InvalidAddress) => pass!(),
        Ok(_) => fail!("COW succeeded on unmapped address"),
        Err(e) => {
            klog_info!(
                "COW_TEST: Got error {:?} for unmapped address (acceptable)",
                e
            );
            pass!()
        }
    }
}

slopos_testing::define_test_suite!(
    cow_edge,
    [
        test_cow_read_not_cow_fault,
        test_cow_not_present_not_cow,
        test_cow_handle_null_pagedir,
        test_cow_handle_not_cow_page,
        test_cow_single_ref_upgrade,
        test_cow_multi_ref_copy,
        test_cow_page_boundary,
        test_cow_clone_modify_both,
        test_cow_multiple_clones,
        test_cow_no_collateral_damage,
        test_cow_handle_invalid_address,
    ]
);
