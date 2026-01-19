//! Demand Paging - Lazy page allocation on first access
//!
//! This module implements on-demand page allocation for anonymous memory regions.
//! When a process accesses a page marked as LAZY in its VMA, the page fault handler
//! calls into this module to allocate a physical page and map it.

use slopos_abi::addr::VirtAddr;

use crate::mm_constants::PAGE_SIZE_4KB;
use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame};
use crate::paging::{ProcessPageDir, map_page_4kb_in_dir, virt_to_phys_in_dir};
use crate::process_vm;
use crate::tlb;
use crate::vma_flags::VmaFlags;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandError {
    NoVma,
    NotDemandPaged,
    AllocationFailed,
    MappingFailed,
    PermissionDenied,
    NullPageDir,
}

pub fn is_demand_fault(error_code: u64, process_id: u32, fault_addr: u64) -> bool {
    let is_present = (error_code & 0x01) != 0;
    if is_present {
        return false;
    }

    let vma_flags = process_vm::process_vm_get_vma_flags(process_id, fault_addr);
    if vma_flags.is_none() {
        return false;
    }

    let flags = vma_flags.unwrap();
    flags.is_demand_paged() && flags.is_anonymous()
}

pub fn can_satisfy_fault(error_code: u64, vma_flags: VmaFlags) -> bool {
    let is_write = (error_code & 0x02) != 0;
    let is_user = (error_code & 0x04) != 0;
    let is_ifetch = (error_code & 0x10) != 0;

    if is_user && !vma_flags.is_user() {
        return false;
    }

    if is_write && !vma_flags.is_writable() {
        return false;
    }

    if is_ifetch && !vma_flags.contains(VmaFlags::EXEC) {
        return false;
    }

    true
}

pub fn handle_demand_fault(
    page_dir: *mut ProcessPageDir,
    process_id: u32,
    fault_addr: u64,
    error_code: u64,
) -> Result<(), DemandError> {
    if page_dir.is_null() {
        return Err(DemandError::NullPageDir);
    }

    let aligned_addr = fault_addr & !(PAGE_SIZE_4KB - 1);

    let vma_flags =
        process_vm::process_vm_get_vma_flags(process_id, aligned_addr).ok_or(DemandError::NoVma)?;

    if !vma_flags.is_demand_paged() || !vma_flags.is_anonymous() {
        return Err(DemandError::NotDemandPaged);
    }

    if !can_satisfy_fault(error_code, vma_flags) {
        return Err(DemandError::PermissionDenied);
    }

    let existing_phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(aligned_addr));
    if !existing_phys.is_null() {
        return Ok(());
    }

    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        return Err(DemandError::AllocationFailed);
    }

    let pte_flags = vma_flags.to_page_flags().bits();
    if map_page_4kb_in_dir(page_dir, VirtAddr::new(aligned_addr), phys, pte_flags) != 0 {
        free_page_frame(phys);
        return Err(DemandError::MappingFailed);
    }

    tlb::flush_page(VirtAddr::new(aligned_addr));

    process_vm::process_vm_increment_pages(process_id, 1);

    Ok(())
}
