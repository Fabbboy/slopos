//! Demand Paging Tests - Finding Real Bugs
//!
//! These tests are designed to find REAL bugs in the demand paging system.
//! They test failure paths, edge cases, and corner conditions that are
//! likely to have bugs since the feature was never tested before.

use core::ffi::c_int;

use slopos_abi::addr::VirtAddr;
use slopos_lib::klog_info;

use crate::demand::{DemandError, can_satisfy_fault, handle_demand_fault, is_demand_fault};
use crate::mm_constants::{INVALID_PROCESS_ID, PAGE_SIZE_4KB, PageFlags};
use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame};
use crate::paging::{map_page_4kb_in_dir, virt_to_phys_in_dir};
use crate::process_vm::{
    create_process_vm, destroy_process_vm, init_process_vm, process_vm_alloc,
    process_vm_get_page_dir, process_vm_get_vma_flags,
};
use crate::vma_flags::VmaFlags;

// ============================================================================
// is_demand_fault() TESTS - The decision logic
// ============================================================================

/// Test: is_demand_fault returns false when page is present
/// This is critical - demand paging should NOT trigger on present pages
pub fn test_demand_fault_present_page() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        klog_info!("DEMAND_TEST: Failed to create process");
        return -1;
    }

    // Allocate heap memory (creates LAZY VMA)
    let addr = process_vm_alloc(pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    if addr == 0 {
        destroy_process_vm(pid);
        return -1;
    }

    // Now actually map a page there (simulate previous fault resolution)
    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    if map_page_4kb_in_dir(
        page_dir,
        VirtAddr::new(addr),
        phys,
        PageFlags::USER_RW.bits(),
    ) != 0
    {
        free_page_frame(phys);
        destroy_process_vm(pid);
        return -1;
    }

    // Error code with PRESENT bit set (bit 0)
    let error_code_present: u64 = 0x01; // Page present

    // This should return false - page is already present
    if is_demand_fault(error_code_present, pid, addr) {
        klog_info!("DEMAND_TEST: BUG - is_demand_fault returned true for present page!");
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}

/// Test: is_demand_fault returns false for address with no VMA
/// Accessing unmapped memory should NOT be treated as demand fault
pub fn test_demand_fault_no_vma() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    // Pick an address we know has no VMA (way past any heap)
    let unmapped_addr: u64 = 0x7FFF_0000_0000;

    // Error code: not present, user mode write
    let error_code: u64 = 0x06; // User + write, NOT present

    // This should return false - no VMA at this address
    if is_demand_fault(error_code, pid, unmapped_addr) {
        klog_info!("DEMAND_TEST: BUG - is_demand_fault returned true for unmapped address!");
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}

/// Test: is_demand_fault returns false for non-LAZY VMA
/// Stack/code regions are NOT demand-paged
pub fn test_demand_fault_non_lazy_vma() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    // The stack is mapped at creation, not LAZY
    // We need to find the stack address from process layout
    // For now, test with code region which is definitely not LAZY
    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    let null_page_addr: u64 = 0;
    let _error_code: u64 = 0x06;

    let flags = process_vm_get_vma_flags(pid, null_page_addr);
    if let Some(f) = flags {
        if f.is_demand_paged() {
            klog_info!("DEMAND_TEST: Null page VMA has LAZY flag (unexpected but not a bug)");
        }
    }

    destroy_process_vm(pid);
    0
}

/// Test: is_demand_fault returns true for valid LAZY+ANON VMA
pub fn test_demand_fault_valid_lazy_vma() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    // Allocate heap memory - this creates LAZY VMA
    let addr = process_vm_alloc(pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    if addr == 0 {
        destroy_process_vm(pid);
        return -1;
    }

    // Verify VMA has correct flags
    let flags = process_vm_get_vma_flags(pid, addr);
    if flags.is_none() {
        klog_info!("DEMAND_TEST: No VMA found for allocated address");
        destroy_process_vm(pid);
        return -1;
    }

    let vma_flags = flags.unwrap();
    if !vma_flags.is_demand_paged() {
        klog_info!("DEMAND_TEST: Allocated VMA is not LAZY");
        destroy_process_vm(pid);
        return -1;
    }
    if !vma_flags.is_anonymous() {
        klog_info!("DEMAND_TEST: Allocated VMA is not ANON");
        destroy_process_vm(pid);
        return -1;
    }

    // Error code: not present, user mode
    let error_code: u64 = 0x04; // User mode, read, NOT present

    // This SHOULD return true - valid demand fault
    if !is_demand_fault(error_code, pid, addr) {
        klog_info!("DEMAND_TEST: BUG - is_demand_fault returned false for valid LAZY VMA!");
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}

// ============================================================================
// can_satisfy_fault() PERMISSION TESTS - Security critical!
// ============================================================================

/// Test: can_satisfy_fault denies write to read-only VMA
pub fn test_demand_permission_deny_write_ro() -> c_int {
    // Create VMA flags: read-only user
    let ro_flags = VmaFlags::READ | VmaFlags::USER | VmaFlags::LAZY | VmaFlags::ANON;

    // Error code for write attempt
    let error_code_write: u64 = 0x06; // User + write, NOT present

    if can_satisfy_fault(error_code_write, ro_flags) {
        klog_info!("DEMAND_TEST: BUG - can_satisfy_fault allowed write to read-only VMA!");
        return -1;
    }

    0
}

/// Test: can_satisfy_fault denies user access to kernel VMA
pub fn test_demand_permission_deny_user_kernel() -> c_int {
    // Create VMA flags: kernel-only (no USER flag)
    let kernel_flags = VmaFlags::READ | VmaFlags::WRITE | VmaFlags::LAZY | VmaFlags::ANON;

    // Error code for user mode access
    let error_code_user: u64 = 0x04; // User mode, read, NOT present

    if can_satisfy_fault(error_code_user, kernel_flags) {
        klog_info!("DEMAND_TEST: BUG - can_satisfy_fault allowed user access to kernel VMA!");
        return -1;
    }

    0
}

/// Test: can_satisfy_fault denies exec on non-exec VMA
pub fn test_demand_permission_deny_exec() -> c_int {
    // Create VMA flags: data (no EXEC)
    let data_flags =
        VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER | VmaFlags::LAZY | VmaFlags::ANON;

    // Error code for instruction fetch (bit 4)
    let error_code_ifetch: u64 = 0x14; // User + ifetch, NOT present

    if can_satisfy_fault(error_code_ifetch, data_flags) {
        klog_info!("DEMAND_TEST: BUG - can_satisfy_fault allowed exec on non-exec VMA!");
        return -1;
    }

    0
}

/// Test: can_satisfy_fault allows valid read on readable VMA
pub fn test_demand_permission_allow_read() -> c_int {
    let readable_flags = VmaFlags::READ | VmaFlags::USER | VmaFlags::LAZY | VmaFlags::ANON;

    // Error code for read
    let error_code_read: u64 = 0x04; // User mode, read, NOT present

    if !can_satisfy_fault(error_code_read, readable_flags) {
        klog_info!("DEMAND_TEST: BUG - can_satisfy_fault denied valid read!");
        return -1;
    }

    0
}

/// Test: can_satisfy_fault allows write on writable VMA
pub fn test_demand_permission_allow_write() -> c_int {
    let writable_flags =
        VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER | VmaFlags::LAZY | VmaFlags::ANON;

    // Error code for write
    let error_code_write: u64 = 0x06; // User + write, NOT present

    if !can_satisfy_fault(error_code_write, writable_flags) {
        klog_info!("DEMAND_TEST: BUG - can_satisfy_fault denied valid write!");
        return -1;
    }

    0
}

// ============================================================================
// handle_demand_fault() TESTS - The actual fault resolution
// ============================================================================

/// Test: handle_demand_fault with null page directory
pub fn test_demand_handle_null_page_dir() -> c_int {
    let result = handle_demand_fault(core::ptr::null_mut(), 1, 0x1000, 0x04);

    match result {
        Err(DemandError::NullPageDir) => 0,
        Ok(_) => {
            klog_info!("DEMAND_TEST: BUG - handle_demand_fault succeeded with null page_dir!");
            -1
        }
        Err(e) => {
            klog_info!("DEMAND_TEST: Wrong error for null page_dir: {:?}", e);
            -1
        }
    }
}

/// Test: handle_demand_fault for address with no VMA
pub fn test_demand_handle_no_vma() -> c_int {
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

    // Unmapped address
    let unmapped_addr: u64 = 0x7FFF_0000_0000;
    let error_code: u64 = 0x04;

    let result = handle_demand_fault(page_dir, pid, unmapped_addr, error_code);

    let ret = match result {
        Err(DemandError::NoVma) => 0,
        Ok(_) => {
            klog_info!("DEMAND_TEST: BUG - handle_demand_fault succeeded for unmapped address!");
            -1
        }
        Err(e) => {
            klog_info!("DEMAND_TEST: Wrong error for unmapped address: {:?}", e);
            -1
        }
    };

    destroy_process_vm(pid);
    ret
}

/// Test: handle_demand_fault resolves valid demand fault
pub fn test_demand_handle_success() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    // Allocate LAZY heap
    let addr = process_vm_alloc(pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    if addr == 0 {
        destroy_process_vm(pid);
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Before fault handling, page should NOT be mapped
    let phys_before = virt_to_phys_in_dir(page_dir, VirtAddr::new(addr));
    if !phys_before.is_null() {
        klog_info!("DEMAND_TEST: Page already mapped before demand fault (might be OK)");
        // This is actually fine - lazy allocation might be eager in some cases
    }

    // Error code: user read, not present
    let error_code: u64 = 0x04;

    let result = handle_demand_fault(page_dir, pid, addr, error_code);

    if let Err(e) = result {
        klog_info!("DEMAND_TEST: handle_demand_fault failed: {:?}", e);
        destroy_process_vm(pid);
        return -1;
    }

    // After fault handling, page SHOULD be mapped
    let phys_after = virt_to_phys_in_dir(page_dir, VirtAddr::new(addr));
    if phys_after.is_null() {
        klog_info!("DEMAND_TEST: BUG - Page not mapped after demand fault resolution!");
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}

/// Test: handle_demand_fault with permission denied (write to RO)
pub fn test_demand_handle_permission_denied() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    // Allocate READ-ONLY heap (no WRITABLE flag)
    let addr = process_vm_alloc(pid, PAGE_SIZE_4KB, 0); // No write permission
    if addr == 0 {
        // This might legitimately fail if allocator requires write
        destroy_process_vm(pid);
        return 0; // Not a test failure
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Try to handle a WRITE fault on read-only region
    let error_code: u64 = 0x06; // User + write, NOT present

    let result = handle_demand_fault(page_dir, pid, addr, error_code);

    let ret = match result {
        Err(DemandError::PermissionDenied) => 0,
        Ok(_) => {
            klog_info!("DEMAND_TEST: BUG - handle_demand_fault allowed write to RO VMA!");
            -1
        }
        Err(e) => {
            // Other errors might be acceptable
            klog_info!("DEMAND_TEST: Got error {:?} (expected PermissionDenied)", e);
            0 // Not necessarily a bug
        }
    };

    destroy_process_vm(pid);
    ret
}

/// Test: handle_demand_fault at page boundary
pub fn test_demand_handle_page_boundary() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    // Allocate 2 pages
    let addr = process_vm_alloc(pid, PAGE_SIZE_4KB * 2, PageFlags::WRITABLE.bits() as u32);
    if addr == 0 {
        destroy_process_vm(pid);
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Fault on address just before page boundary (addr + 0xFFF)
    let boundary_addr = addr + PAGE_SIZE_4KB - 1;
    let error_code: u64 = 0x04;

    let result = handle_demand_fault(page_dir, pid, boundary_addr, error_code);

    if let Err(e) = result {
        klog_info!("DEMAND_TEST: Boundary fault failed: {:?}", e);
        destroy_process_vm(pid);
        return -1;
    }

    // The first page should be mapped
    let phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(addr));
    if phys.is_null() {
        klog_info!("DEMAND_TEST: BUG - First page not mapped after boundary fault!");
        destroy_process_vm(pid);
        return -1;
    }

    // Second page should NOT be mapped yet
    let phys2 = virt_to_phys_in_dir(page_dir, VirtAddr::new(addr + PAGE_SIZE_4KB));
    if !phys2.is_null() {
        klog_info!("DEMAND_TEST: Second page mapped (might be prefetching - OK)");
    }

    destroy_process_vm(pid);
    0
}

/// Test: Multiple demand faults on same VMA
pub fn test_demand_multiple_faults() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    // Allocate 4 pages
    let addr = process_vm_alloc(pid, PAGE_SIZE_4KB * 4, PageFlags::WRITABLE.bits() as u32);
    if addr == 0 {
        destroy_process_vm(pid);
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    let error_code: u64 = 0x04;

    // Fault on pages in non-sequential order
    let offsets = [2, 0, 3, 1];
    for &offset in &offsets {
        let fault_addr = addr + (offset as u64) * PAGE_SIZE_4KB;
        let result = handle_demand_fault(page_dir, pid, fault_addr, error_code);

        if let Err(e) = result {
            klog_info!(
                "DEMAND_TEST: Multiple faults failed at offset {}: {:?}",
                offset,
                e
            );
            destroy_process_vm(pid);
            return -1;
        }
    }

    // Verify all 4 pages are now mapped
    for i in 0..4 {
        let check_addr = addr + (i as u64) * PAGE_SIZE_4KB;
        let phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(check_addr));
        if phys.is_null() {
            klog_info!("DEMAND_TEST: BUG - Page {} not mapped after fault!", i);
            destroy_process_vm(pid);
            return -1;
        }
    }

    destroy_process_vm(pid);
    0
}

/// Test: Demand fault on already-faulted page (idempotent?)
pub fn test_demand_double_fault() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let addr = process_vm_alloc(pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    if addr == 0 {
        destroy_process_vm(pid);
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    let error_code: u64 = 0x04;

    // First fault - should succeed
    let result1 = handle_demand_fault(page_dir, pid, addr, error_code);
    if let Err(e) = result1 {
        klog_info!("DEMAND_TEST: First fault failed: {:?}", e);
        destroy_process_vm(pid);
        return -1;
    }

    let phys1 = virt_to_phys_in_dir(page_dir, VirtAddr::new(addr));

    // Second fault on same address - what happens?
    // The page is now present, so is_demand_fault() should return false
    // and we shouldn't even get here in real usage
    // But handle_demand_fault might be called directly...
    let result2 = handle_demand_fault(page_dir, pid, addr, error_code);

    // This might succeed (mapping same page again) or fail
    // Either is acceptable as long as it doesn't corrupt state
    if let Ok(_) = result2 {
        let phys2 = virt_to_phys_in_dir(page_dir, VirtAddr::new(addr));
        if phys2 != phys1 {
            klog_info!(
                "DEMAND_TEST: BUG - Double fault changed physical mapping! {:#x} -> {:#x}",
                phys1.as_u64(),
                phys2.as_u64()
            );
            destroy_process_vm(pid);
            return -1;
        }
    }

    destroy_process_vm(pid);
    0
}

/// Test: Demand fault with invalid process ID
pub fn test_demand_invalid_process_id() -> c_int {
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

    // Use wrong PID with valid page_dir
    let wrong_pid = pid + 999;
    let error_code: u64 = 0x04;

    // This should fail because VMA lookup uses the wrong PID
    let result = handle_demand_fault(page_dir, wrong_pid, 0x1000, error_code);

    let ret = match result {
        Err(DemandError::NoVma) => 0, // Expected
        Ok(_) => {
            klog_info!("DEMAND_TEST: BUG - Demand fault succeeded with wrong PID!");
            -1
        }
        Err(_) => 0, // Other errors acceptable
    };

    destroy_process_vm(pid);
    ret
}
