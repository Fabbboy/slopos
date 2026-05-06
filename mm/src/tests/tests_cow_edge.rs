use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};
use slopos_utils::klog_info;

use crate::cow::is_cow_fault;
use crate::dual_paging::ostd_map_4kb_user;
use crate::error::MmError;
use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::{process_vm_clone_cow, process_vm_with_dual_paging};
use crate::tests::test_fixtures::ProcessVmGuard;
use slopos_abi::addr::VirtAddr;
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_ostd::mm::frame::{AnonymousMeta, Frame, Paddr, reference_count_at};

pub fn test_cow_read_not_cow_fault() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let Some(_phys) = vm.map_test_page(0x2000, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    vm.mark_cow(0x2000);

    let error_code_read: u64 = 0x05;
    let cow_fault =
        process_vm_with_dual_paging(vm.pid, |_pd, vs| is_cow_fault(error_code_read, vs, 0x2000))
            .unwrap_or(false);
    assert_test!(!cow_fault, "is_cow_fault returned true for read access");

    pass!()
}

pub fn test_cow_not_present_not_cow() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let unmapped_addr: u64 = 0x5000_0000;
    let error_code: u64 = 0x02;

    let cow_fault = process_vm_with_dual_paging(vm.pid, |_pd, vs| {
        is_cow_fault(error_code, vs, unmapped_addr)
    })
    .unwrap_or(false);
    assert_test!(
        !cow_fault,
        "is_cow_fault returned true for not-present page"
    );

    pass!()
}

pub fn test_cow_handle_null_pagedir() -> TestResult {
    // Public dispatcher returns `None` for an unknown PID — that's the
    // failure path callers actually hit post-framekernel.
    let result = crate::process_vm::process_vm_with_dual_paging(INVALID_PROCESS_ID, |_, _| ());
    if result.is_some() {
        return fail!("process_vm_with_dual_paging returned Some for INVALID_PROCESS_ID");
    }
    pass!()
}

pub fn test_cow_handle_not_cow_page() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let Some(_phys) = vm.map_test_page(0x3000, PageFlags::USER_RW.bits()) else {
        return fail!("map test page");
    };

    match vm.handle_cow_fault(0x3000) {
        Err(MmError::NotCowPage) => pass!(),
        Ok(_) => fail!("handle_cow_fault succeeded on non-COW page"),
        Err(e) => fail!("wrong error for non-COW page: {:?}", e),
    }
}

pub fn test_cow_single_ref_upgrade() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let Some(phys) = vm.map_test_page(0x4000, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    vm.mark_cow(0x4000);

    let ref_before = reference_count_at(Paddr::new(phys.as_u64()));
    assert_test!(ref_before == 1, "initial META_SLOTS refcount should be 1");

    if let Err(e) = vm.handle_cow_fault(0x4000) {
        return fail!("single-ref COW failed: {:?}", e);
    }

    let phys_after = vm.virt_to_phys(0x4000);
    if phys_after != phys {
        klog_info!(
            "COW_TEST: Single-ref COW copied page unnecessarily! {:#x} -> {:#x}",
            phys.as_u64(),
            phys_after.as_u64()
        );
    }

    assert_test!(!vm.is_cow(0x4000), "page still marked COW after resolution");

    pass!()
}

pub fn test_cow_multi_ref_copy() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let test_addr: u64 = 0x5000;
    let Some(phys) = vm.map_test_page(test_addr, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..PAGE_SIZE_4KB as usize {
            unsafe { *ptr.add(i) = (i & 0xFF) as u8 };
        }
    }

    vm.mark_cow(test_addr);

    // Bump META_SLOTS refcount directly via OSTD's `Frame::from_in_use`
    // — equivalent to two extra processes mapping the same paddr.
    // Hold the extra refs in a scope so they drop together below.
    let extra1 = Frame::<AnonymousMeta>::from_in_use(Paddr::new(phys.as_u64()))
        .expect("from_in_use 1 for COW multi-ref test");
    let extra2 = Frame::<AnonymousMeta>::from_in_use(Paddr::new(phys.as_u64()))
        .expect("from_in_use 2 for COW multi-ref test");

    let ref_before = reference_count_at(Paddr::new(phys.as_u64()));
    if ref_before < 3 {
        klog_info!("COW_TEST: Expected refcount >=3, got {}", ref_before);
    }

    if let Err(e) = vm.handle_cow_fault(test_addr) {
        drop(extra1);
        drop(extra2);
        return fail!("multi-ref COW failed: {:?}", e);
    }

    let phys_after = vm.virt_to_phys(test_addr);
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

    let ref_after = reference_count_at(Paddr::new(phys.as_u64()));
    assert_test!(ref_after < ref_before, "old page refcount didn't decrement");

    drop(extra1);
    drop(extra2);

    pass!()
}

pub fn test_cow_page_boundary() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let page_start: u64 = 0x6000;
    let Some(_phys) = vm.map_test_page(page_start, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    vm.mark_cow(page_start);

    let fault_addr = page_start + PAGE_SIZE_4KB - 1;
    if let Err(e) = vm.handle_cow_fault(fault_addr) {
        return fail!("boundary COW failed: {:?}", e);
    }

    pass!()
}

pub fn test_cow_clone_modify_both() -> TestResult {
    use crate::process_vm::process_vm_alloc;

    let Some(parent) = ProcessVmGuard::new() else {
        return fail!("create parent VM");
    };

    let test_addr = process_vm_alloc(parent.pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(test_addr != 0, "process_vm_alloc failed");

    let Some(phys) = parent.map_test_page(test_addr, PageFlags::USER_RW.bits()) else {
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

    if parent.is_cow(test_addr) {
        if let Err(e) = parent.handle_cow_fault(test_addr) {
            return fail!("parent COW resolution failed: {:?}", e);
        }
    }

    let parent_phys = parent.virt_to_phys(test_addr);
    if let Some(virt) = parent_phys.to_virt_checked() {
        unsafe { *virt.as_mut_ptr::<u8>() = 0xBB };
    }

    if child.is_cow(test_addr) {
        if let Err(e) = child.handle_cow_fault(test_addr) {
            return fail!("child COW resolution failed: {:?}", e);
        }
    }

    let child_phys = child.virt_to_phys(test_addr);
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

    let map_addr1 = process_vm_with_dual_paging(vm.pid, |_pd, vs| {
        ostd_map_4kb_user(vs, VirtAddr::new(addr1), phys1, PageFlags::USER_RO.bits())
    });
    if !matches!(map_addr1, Some(Ok(()))) {
        free_page_frame(phys1);
        free_page_frame(phys2);
        return fail!("map page 1");
    }
    let map_addr2 = process_vm_with_dual_paging(vm.pid, |_pd, vs| {
        ostd_map_4kb_user(vs, VirtAddr::new(addr2), phys2, PageFlags::USER_RO.bits())
    });
    if !matches!(map_addr2, Some(Ok(()))) {
        free_page_frame(phys2);
        return fail!("map page 2");
    }

    vm.mark_cow(addr1);
    vm.mark_cow(addr2);

    if let Err(e) = vm.handle_cow_fault(addr1) {
        return fail!("first page COW failed: {:?}", e);
    }

    let phys2_after = vm.virt_to_phys(addr2);
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
    match vm.handle_cow_fault(unmapped) {
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

slopos_testing::stest!(name = test_cow_read_not_cow_fault, suite = cow_edge);
slopos_testing::stest!(name = test_cow_not_present_not_cow, suite = cow_edge);
slopos_testing::stest!(name = test_cow_handle_null_pagedir, suite = cow_edge);
slopos_testing::stest!(name = test_cow_handle_not_cow_page, suite = cow_edge);
slopos_testing::stest!(name = test_cow_single_ref_upgrade, suite = cow_edge);
slopos_testing::stest!(name = test_cow_multi_ref_copy, suite = cow_edge);
slopos_testing::stest!(name = test_cow_page_boundary, suite = cow_edge);
slopos_testing::stest!(name = test_cow_clone_modify_both, suite = cow_edge);
slopos_testing::stest!(name = test_cow_multiple_clones, suite = cow_edge);
slopos_testing::stest!(name = test_cow_no_collateral_damage, suite = cow_edge);
slopos_testing::stest!(name = test_cow_handle_invalid_address, suite = cow_edge);
