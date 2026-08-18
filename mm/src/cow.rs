use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::frame::{Paddr, reference_count_at};
use slopos_ostd::mm::vm_space::VmSpace;

use crate::error::MmError;
use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::tlb;
use crate::user_mappings::{
    ostd_get_pte_flags_4kb, ostd_map_4kb_user, ostd_resolve_cow_4kb, ostd_unmap_4kb_user,
    ostd_virt_to_phys_4kb,
};

/// Copy a full 4 KiB page through the HHDM mapping. Both `src` and `dst`
/// must be live HHDM-mapped virtual addresses pointing at distinct pages.
#[inline]
fn copy_full_page(src: VirtAddr, dst: VirtAddr) {
    let _ = slopos_ostd::mm::hhdm_bytes::copy_page(src, dst);
}

pub fn handle_cow_fault(vm_space: &mut KArc<VmSpace>, fault_addr: u64) -> Result<(), MmError> {
    let vaddr = VirtAddr::new(fault_addr);
    let aligned_vaddr = VirtAddr::new(fault_addr & !(PAGE_SIZE_4KB - 1));

    let flags = match ostd_get_pte_flags_4kb(vm_space, vaddr) {
        Some(f) if f.contains(PageFlags::COW) => f,
        _ => return Err(MmError::NotCowPage),
    };
    let _ = flags;

    let old_phys = ostd_virt_to_phys_4kb(vm_space, aligned_vaddr);
    if old_phys.is_null() {
        return Err(MmError::InvalidAddress);
    }

    let ref_count = reference_count_at(Paddr::new(old_phys.as_u64() & !(PAGE_SIZE_4KB - 1)));

    if ref_count <= 1 {
        return resolve_single_ref(vm_space, aligned_vaddr);
    }

    resolve_multi_ref(vm_space, aligned_vaddr, old_phys)
}

fn resolve_single_ref(
    vm_space: &mut KArc<VmSpace>,
    aligned_vaddr: VirtAddr,
) -> Result<(), MmError> {
    if !ostd_resolve_cow_4kb(vm_space, aligned_vaddr).map_err(|_| MmError::MappingFailed)? {
        return Err(MmError::MappingFailed);
    }
    Ok(())
}

fn resolve_multi_ref(
    vm_space: &mut KArc<VmSpace>,
    aligned_vaddr: VirtAddr,
    old_phys: PhysAddr,
) -> Result<(), MmError> {
    let new_phys = alloc_kernel_page();
    if new_phys.is_null() {
        return Err(MmError::NoMemory);
    }

    let old_virt = old_phys.to_virt();
    let new_virt = new_phys.to_virt();

    if old_virt.is_null() || new_virt.is_null() {
        free_page_frame(new_phys);
        return Err(MmError::InvalidAddress);
    }

    copy_full_page(old_virt, new_virt);

    let new_flags = PageFlags::USER_RW;

    if let Err(err) = ostd_unmap_4kb_user(vm_space, aligned_vaddr) {
        slopos_ostd::klog_info!("cow::resolve_multi_ref: OSTD unmap failed: {:?}", err);
        free_page_frame(new_phys);
        return Err(MmError::MappingFailed);
    }
    if let Err(err) = ostd_map_4kb_user(vm_space, aligned_vaddr, new_phys, new_flags.bits()) {
        slopos_ostd::klog_info!("cow::resolve_multi_ref: OSTD remap failed: {:?}", err);
        free_page_frame(new_phys);
        return Err(MmError::MappingFailed);
    }

    tlb::flush_page(aligned_vaddr);

    Ok(())
}

pub fn is_cow_fault(error_code: u64, vm_space: &KArc<VmSpace>, fault_addr: u64) -> bool {
    let is_write = (error_code & 0x02) != 0;
    let is_present = (error_code & 0x01) != 0;

    if !is_write || !is_present {
        return false;
    }

    ostd_get_pte_flags_4kb(vm_space, VirtAddr::new(fault_addr))
        .map_or(false, |f| f.contains(PageFlags::COW))
}
