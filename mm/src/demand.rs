//! Demand Paging - Lazy page allocation on first access
//!
//! When a process accesses a page in a lazy-anonymous VMA, the page fault
//! handler calls into this module to allocate a physical page and map it.

use slopos_abi::addr::VirtAddr;
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::vm_space::VmSpace;

use crate::dual_paging::ostd_map_4kb_user;
use crate::error::MmError;
use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame};
use crate::paging::{ProcessPageDir, map_page_4kb_in_dir, unmap_page_in_dir, virt_to_phys_in_dir};
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::process_vm;
use crate::tlb;
use crate::vma_region::VmaRegion;

pub fn is_demand_fault(error_code: u64, process_id: u32, fault_addr: u64) -> bool {
    let is_present = (error_code & 0x01) != 0;
    if is_present {
        return false;
    }

    let Some(region) = process_vm::process_vm_get_region(process_id, fault_addr) else {
        return false;
    };

    region.is_demand_paged() && region.is_anonymous()
}

pub fn can_satisfy_fault(error_code: u64, region: &VmaRegion) -> bool {
    let is_write = (error_code & 0x02) != 0;
    let is_user = (error_code & 0x04) != 0;
    let is_ifetch = (error_code & 0x10) != 0;

    if is_user && !region.user {
        return false;
    }

    if is_write && !region.protection.write {
        return false;
    }

    if is_ifetch && !region.protection.exec {
        return false;
    }

    true
}

pub fn handle_demand_fault(
    page_dir: *mut ProcessPageDir,
    vm_space: &mut KArc<VmSpace>,
    process_id: u32,
    fault_addr: u64,
    error_code: u64,
    region: &VmaRegion,
) -> Result<(), MmError> {
    if page_dir.is_null() {
        return Err(MmError::NullPageDir);
    }

    let aligned_addr = fault_addr & !(PAGE_SIZE_4KB - 1);

    if !region.is_demand_paged() || !region.is_anonymous() {
        return Err(MmError::NotDemandPaged);
    }

    if !can_satisfy_fault(error_code, region) {
        return Err(MmError::PermissionDenied);
    }

    let existing_phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(aligned_addr));
    if !existing_phys.is_null() {
        return Ok(());
    }

    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        return Err(MmError::NoMemory);
    }

    let pte_flags = region.to_page_flags().bits();
    if map_page_4kb_in_dir(page_dir, VirtAddr::new(aligned_addr), phys, pte_flags) != 0 {
        free_page_frame(phys);
        return Err(MmError::MappingFailed);
    }

    if let Err(err) = ostd_map_4kb_user(vm_space, VirtAddr::new(aligned_addr), phys, pte_flags) {
        slopos_utils::klog_info!("demand::handle_demand_fault: OSTD map failed: {:?}", err);
        let leaked = unmap_page_in_dir(page_dir, VirtAddr::new(aligned_addr));
        if !leaked.is_null() {
            free_page_frame(leaked);
        }
        return Err(MmError::MappingFailed);
    }

    tlb::flush_page(VirtAddr::new(aligned_addr));

    // Caller increments process_vm.total_pages outside the per-process
    // lock to avoid recursive lock acquisition; see
    // `page_fault::try_resolve_user_fault`.
    let _ = process_id;
    let _ = process_vm::process_vm_get_page_dir; // silence unused-import lint

    Ok(())
}
