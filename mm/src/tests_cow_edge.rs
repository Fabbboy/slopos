//! COW (Copy-on-Write) Edge Case Tests
//!
//! Tests designed to find bugs in COW handling, particularly failure
//! scenarios and edge cases that are rarely exercised.

use core::ffi::c_int;

use slopos_abi::addr::VirtAddr;
use slopos_lib::klog_info;

use crate::cow::{CowError, handle_cow_fault, is_cow_fault};
use crate::hhdm::PhysAddrHhdm;
use crate::mm_constants::{INVALID_PROCESS_ID, PAGE_SIZE_4KB, PageFlags};
use crate::page_alloc::{
    ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame, page_frame_get_ref, page_frame_inc_ref,
};
use crate::paging::{map_page_4kb_in_dir, paging_is_cow, paging_mark_cow, virt_to_phys_in_dir};
use crate::process_vm::{
    create_process_vm, destroy_process_vm, init_process_vm, process_vm_clone_cow,
    process_vm_get_page_dir,
};

/// Test: is_cow_fault with read access (should return false)
pub fn test_cow_read_not_cow_fault() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    let test_addr: u64 = 0x2000;
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    if map_page_4kb_in_dir(
        page_dir,
        VirtAddr::new(test_addr),
        phys,
        PageFlags::USER_RO.bits(),
    ) != 0
    {
        free_page_frame(phys);
        destroy_process_vm(pid);
        return -1;
    }

    paging_mark_cow(page_dir, VirtAddr::new(test_addr));

    // Error code for READ (bit 1 NOT set)
    let error_code_read: u64 = 0x05; // Present + User, but NOT write

    if is_cow_fault(error_code_read, page_dir, test_addr) {
        klog_info!("COW_TEST: BUG - is_cow_fault returned true for read access!");
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}

/// Test: is_cow_fault with not-present page (should return false)
pub fn test_cow_not_present_not_cow() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Use unmapped address
    let unmapped_addr: u64 = 0x5000_0000;

    // Error code for write to not-present page
    let error_code: u64 = 0x02; // Write, NOT present

    if is_cow_fault(error_code, page_dir, unmapped_addr) {
        klog_info!("COW_TEST: BUG - is_cow_fault returned true for not-present page!");
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}

/// Test: handle_cow_fault with null page directory
pub fn test_cow_handle_null_pagedir() -> c_int {
    let result = handle_cow_fault(core::ptr::null_mut(), 0x1000);

    match result {
        Err(CowError::NullPageDir) => 0,
        Ok(_) => {
            klog_info!("COW_TEST: BUG - handle_cow_fault succeeded with null page_dir!");
            -1
        }
        Err(e) => {
            klog_info!("COW_TEST: Wrong error for null page_dir: {:?}", e);
            -1
        }
    }
}

/// Test: handle_cow_fault on non-COW page
pub fn test_cow_handle_not_cow_page() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    let test_addr: u64 = 0x3000;
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Map as writable (NOT COW)
    if map_page_4kb_in_dir(
        page_dir,
        VirtAddr::new(test_addr),
        phys,
        PageFlags::USER_RW.bits(),
    ) != 0
    {
        free_page_frame(phys);
        destroy_process_vm(pid);
        return -1;
    }

    // Don't mark as COW!
    let result = handle_cow_fault(page_dir, test_addr);

    let ret = match result {
        Err(CowError::NotCowPage) => 0,
        Ok(_) => {
            klog_info!("COW_TEST: BUG - handle_cow_fault succeeded on non-COW page!");
            -1
        }
        Err(e) => {
            klog_info!("COW_TEST: Wrong error for non-COW page: {:?}", e);
            -1
        }
    };

    destroy_process_vm(pid);
    ret
}

/// Test: COW with refcount == 1 (should just upgrade permissions)
pub fn test_cow_single_ref_upgrade() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    let test_addr: u64 = 0x4000;
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Map read-only and mark COW
    if map_page_4kb_in_dir(
        page_dir,
        VirtAddr::new(test_addr),
        phys,
        PageFlags::USER_RO.bits(),
    ) != 0
    {
        free_page_frame(phys);
        destroy_process_vm(pid);
        return -1;
    }

    paging_mark_cow(page_dir, VirtAddr::new(test_addr));

    // Refcount should be 1
    let ref_before = page_frame_get_ref(phys);
    if ref_before != 1 {
        klog_info!("COW_TEST: Initial refcount should be 1, got {}", ref_before);
        destroy_process_vm(pid);
        return -1;
    }

    // Handle COW fault
    let result = handle_cow_fault(page_dir, test_addr);
    if let Err(e) = result {
        klog_info!("COW_TEST: Single-ref COW failed: {:?}", e);
        destroy_process_vm(pid);
        return -1;
    }

    // Physical address should be the SAME (just permission upgrade)
    let phys_after = virt_to_phys_in_dir(page_dir, VirtAddr::new(test_addr));
    if phys_after != phys {
        klog_info!(
            "COW_TEST: Single-ref COW copied page unnecessarily! {:#x} -> {:#x}",
            phys.as_u64(),
            phys_after.as_u64()
        );
        // This isn't necessarily a bug, just inefficient
    }

    // Page should no longer be COW
    if paging_is_cow(page_dir, VirtAddr::new(test_addr)) {
        klog_info!("COW_TEST: BUG - Page still marked COW after resolution!");
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}

/// Test: COW with refcount > 1 (should copy and decrement)
pub fn test_cow_multi_ref_copy() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    let test_addr: u64 = 0x5000;
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Write a pattern to the page
    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..PAGE_SIZE_4KB as usize {
            unsafe {
                *ptr.add(i) = (i & 0xFF) as u8;
            }
        }
    }

    // Map read-only and mark COW
    if map_page_4kb_in_dir(
        page_dir,
        VirtAddr::new(test_addr),
        phys,
        PageFlags::USER_RO.bits(),
    ) != 0
    {
        free_page_frame(phys);
        destroy_process_vm(pid);
        return -1;
    }

    paging_mark_cow(page_dir, VirtAddr::new(test_addr));

    // Artificially increase refcount to simulate sharing
    page_frame_inc_ref(phys);
    page_frame_inc_ref(phys);

    let ref_before = page_frame_get_ref(phys);
    if ref_before != 3 {
        klog_info!("COW_TEST: Expected refcount 3, got {}", ref_before);
    }

    // Handle COW fault
    let result = handle_cow_fault(page_dir, test_addr);
    if let Err(e) = result {
        klog_info!("COW_TEST: Multi-ref COW failed: {:?}", e);
        destroy_process_vm(pid);
        return -1;
    }

    // Physical address should be DIFFERENT (copy made)
    let phys_after = virt_to_phys_in_dir(page_dir, VirtAddr::new(test_addr));
    if phys_after == phys {
        klog_info!("COW_TEST: BUG - Multi-ref COW didn't copy page!");
        destroy_process_vm(pid);
        return -1;
    }

    // Verify data was copied correctly
    if let Some(virt) = phys_after.to_virt_checked() {
        let ptr = virt.as_ptr::<u8>();
        for i in 0..PAGE_SIZE_4KB as usize {
            let val = unsafe { *ptr.add(i) };
            let expected = (i & 0xFF) as u8;
            if val != expected {
                klog_info!(
                    "COW_TEST: BUG - Data not copied correctly at offset {}: expected {:#x}, got {:#x}",
                    i,
                    expected,
                    val
                );
                destroy_process_vm(pid);
                return -1;
            }
        }
    }

    // Old page refcount should have decremented
    let ref_after = page_frame_get_ref(phys);
    if ref_after >= ref_before {
        klog_info!(
            "COW_TEST: BUG - Old page refcount didn't decrement! Before: {}, After: {}",
            ref_before,
            ref_after
        );
        // Clean up remaining refs
        for _ in 0..ref_after {
            free_page_frame(phys);
        }
        destroy_process_vm(pid);
        return -1;
    }

    // Clean up the extra refs we added
    for _ in 0..(ref_after) {
        free_page_frame(phys);
    }

    destroy_process_vm(pid);
    0
}

/// Test: COW on page at address boundary
pub fn test_cow_page_boundary() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Use address just below page boundary
    let page_start: u64 = 0x6000;
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    if map_page_4kb_in_dir(
        page_dir,
        VirtAddr::new(page_start),
        phys,
        PageFlags::USER_RO.bits(),
    ) != 0
    {
        free_page_frame(phys);
        destroy_process_vm(pid);
        return -1;
    }

    paging_mark_cow(page_dir, VirtAddr::new(page_start));

    // Fault at last byte of page
    let fault_addr = page_start + PAGE_SIZE_4KB - 1;

    let result = handle_cow_fault(page_dir, fault_addr);
    if let Err(e) = result {
        klog_info!("COW_TEST: Boundary COW failed: {:?}", e);
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}

/// Test: COW clone then modify both parent and child
pub fn test_cow_clone_modify_both() -> c_int {
    init_process_vm();

    let parent_pid = create_process_vm();
    if parent_pid == INVALID_PROCESS_ID {
        return -1;
    }

    let parent_dir = process_vm_get_page_dir(parent_pid);
    if parent_dir.is_null() {
        destroy_process_vm(parent_pid);
        return -1;
    }

    // Allocate and map a page in parent with distinct pattern
    use crate::process_vm::process_vm_alloc;
    let test_addr = process_vm_alloc(parent_pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    if test_addr == 0 {
        destroy_process_vm(parent_pid);
        return -1;
    }

    // Map a physical page and write parent pattern
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        destroy_process_vm(parent_pid);
        return -1;
    }

    if map_page_4kb_in_dir(
        parent_dir,
        VirtAddr::new(test_addr),
        phys,
        PageFlags::USER_RW.bits(),
    ) != 0
    {
        free_page_frame(phys);
        destroy_process_vm(parent_pid);
        return -1;
    }

    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..PAGE_SIZE_4KB as usize {
            unsafe {
                *ptr.add(i) = 0xAA;
            }
        }
    }

    // Clone with COW
    let child_pid = process_vm_clone_cow(parent_pid);
    if child_pid == INVALID_PROCESS_ID {
        klog_info!("COW_TEST: Clone failed");
        destroy_process_vm(parent_pid);
        return -1;
    }

    let child_dir = process_vm_get_page_dir(child_pid);
    if child_dir.is_null() {
        destroy_process_vm(child_pid);
        destroy_process_vm(parent_pid);
        return -1;
    }

    // Parent should trigger COW if it writes
    if paging_is_cow(parent_dir, VirtAddr::new(test_addr)) {
        let result = handle_cow_fault(parent_dir, test_addr);
        if let Err(e) = result {
            klog_info!("COW_TEST: Parent COW resolution failed: {:?}", e);
            destroy_process_vm(child_pid);
            destroy_process_vm(parent_pid);
            return -1;
        }
    }

    // Write different pattern to parent
    let parent_phys = virt_to_phys_in_dir(parent_dir, VirtAddr::new(test_addr));
    if let Some(virt) = parent_phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        unsafe {
            *ptr = 0xBB;
        }
    }

    // Child should trigger COW if it writes
    if paging_is_cow(child_dir, VirtAddr::new(test_addr)) {
        let result = handle_cow_fault(child_dir, test_addr);
        if let Err(e) = result {
            klog_info!("COW_TEST: Child COW resolution failed: {:?}", e);
            destroy_process_vm(child_pid);
            destroy_process_vm(parent_pid);
            return -1;
        }
    }

    // Write different pattern to child
    let child_phys = virt_to_phys_in_dir(child_dir, VirtAddr::new(test_addr));
    if let Some(virt) = child_phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        unsafe {
            *ptr = 0xCC;
        }
    }

    // Verify isolation - parent and child have different data
    if let (Some(pv), Some(cv)) = (parent_phys.to_virt_checked(), child_phys.to_virt_checked()) {
        let parent_val = unsafe { *pv.as_ptr::<u8>() };
        let child_val = unsafe { *cv.as_ptr::<u8>() };

        if parent_val == child_val {
            klog_info!(
                "COW_TEST: BUG - Parent and child share same data after COW! Both have {:#x}",
                parent_val
            );
            destroy_process_vm(child_pid);
            destroy_process_vm(parent_pid);
            return -1;
        }

        if parent_val != 0xBB {
            klog_info!(
                "COW_TEST: Parent data corrupted: expected 0xBB, got {:#x}",
                parent_val
            );
            destroy_process_vm(child_pid);
            destroy_process_vm(parent_pid);
            return -1;
        }

        if child_val != 0xCC {
            klog_info!(
                "COW_TEST: Child data corrupted: expected 0xCC, got {:#x}",
                child_val
            );
            destroy_process_vm(child_pid);
            destroy_process_vm(parent_pid);
            return -1;
        }
    }

    destroy_process_vm(child_pid);
    destroy_process_vm(parent_pid);
    0
}

/// Test: Multiple COW clones from same parent
pub fn test_cow_multiple_clones() -> c_int {
    init_process_vm();

    let parent_pid = create_process_vm();
    if parent_pid == INVALID_PROCESS_ID {
        return -1;
    }

    // Clone multiple children
    let mut children: [u32; 4] = [INVALID_PROCESS_ID; 4];
    let mut child_count = 0usize;

    for i in 0..4 {
        let child_pid = process_vm_clone_cow(parent_pid);
        if child_pid == INVALID_PROCESS_ID {
            klog_info!("COW_TEST: Clone {} failed", i);
            break;
        }
        children[i] = child_pid;
        child_count += 1;
    }

    if child_count < 2 {
        klog_info!("COW_TEST: Couldn't create enough clones");
        for i in 0..child_count {
            destroy_process_vm(children[i]);
        }
        destroy_process_vm(parent_pid);
        return -1;
    }

    // Clean up
    for i in 0..child_count {
        destroy_process_vm(children[i]);
    }
    destroy_process_vm(parent_pid);

    0
}

/// Test: COW fault resolution doesn't corrupt other mappings
pub fn test_cow_no_collateral_damage() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Map two adjacent pages
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
        destroy_process_vm(pid);
        return -1;
    }

    // Write distinct patterns
    if let Some(v1) = phys1.to_virt_checked() {
        unsafe {
            core::ptr::write_bytes(v1.as_mut_ptr::<u8>(), 0x11, PAGE_SIZE_4KB as usize);
        }
    }
    if let Some(v2) = phys2.to_virt_checked() {
        unsafe {
            core::ptr::write_bytes(v2.as_mut_ptr::<u8>(), 0x22, PAGE_SIZE_4KB as usize);
        }
    }

    // Map both as COW
    if map_page_4kb_in_dir(
        page_dir,
        VirtAddr::new(addr1),
        phys1,
        PageFlags::USER_RO.bits(),
    ) != 0
    {
        free_page_frame(phys1);
        free_page_frame(phys2);
        destroy_process_vm(pid);
        return -1;
    }
    if map_page_4kb_in_dir(
        page_dir,
        VirtAddr::new(addr2),
        phys2,
        PageFlags::USER_RO.bits(),
    ) != 0
    {
        free_page_frame(phys2);
        destroy_process_vm(pid);
        return -1;
    }

    paging_mark_cow(page_dir, VirtAddr::new(addr1));
    paging_mark_cow(page_dir, VirtAddr::new(addr2));

    // Resolve COW on first page only
    let result = handle_cow_fault(page_dir, addr1);
    if let Err(e) = result {
        klog_info!("COW_TEST: First page COW failed: {:?}", e);
        destroy_process_vm(pid);
        return -1;
    }

    // Verify second page is UNCHANGED
    let phys2_after = virt_to_phys_in_dir(page_dir, VirtAddr::new(addr2));
    if phys2_after != phys2 {
        klog_info!(
            "COW_TEST: BUG - Second page physical address changed! {:#x} -> {:#x}",
            phys2.as_u64(),
            phys2_after.as_u64()
        );
        destroy_process_vm(pid);
        return -1;
    }

    // Verify second page data is unchanged
    if let Some(v2) = phys2_after.to_virt_checked() {
        let val = unsafe { *v2.as_ptr::<u8>() };
        if val != 0x22 {
            klog_info!(
                "COW_TEST: BUG - Second page data corrupted: expected 0x22, got {:#x}",
                val
            );
            destroy_process_vm(pid);
            return -1;
        }
    }

    destroy_process_vm(pid);
    0
}

/// Test: COW with invalid/unmapped address
pub fn test_cow_handle_invalid_address() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Try COW on unmapped address
    let unmapped: u64 = 0xDEAD_0000;

    let result = handle_cow_fault(page_dir, unmapped);

    let ret = match result {
        Err(CowError::NotCowPage) | Err(CowError::InvalidAddress) => 0,
        Ok(_) => {
            klog_info!("COW_TEST: BUG - COW succeeded on unmapped address!");
            -1
        }
        Err(e) => {
            klog_info!(
                "COW_TEST: Got error {:?} for unmapped address (acceptable)",
                e
            );
            0
        }
    };

    destroy_process_vm(pid);
    ret
}
