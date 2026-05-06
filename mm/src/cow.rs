use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::frame::{Paddr, reference_count_at};
use slopos_ostd::mm::vm_space::VmSpace;

use crate::dual_paging::{
    ostd_get_pte_flags_4kb, ostd_map_4kb_user, ostd_resolve_cow_4kb, ostd_unmap_4kb_user,
    ostd_virt_to_phys_4kb,
};
use crate::error::MmError;
use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::tlb;

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
    // Sole owner: just flip PTE flags in-place (clear COW software bit,
    // set WRITABLE). `ostd_resolve_cow_4kb` does that atomically through
    // the cursor's `protect::<Size4Kb>` and never disturbs the backing
    // frame.
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
    let new_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if new_phys.is_null() {
        return Err(MmError::NoMemory);
    }

    let old_virt = old_phys.to_virt();
    let new_virt = new_phys.to_virt();

    if old_virt.is_null() || new_virt.is_null() {
        free_page_frame(new_phys);
        return Err(MmError::InvalidAddress);
    }

    unsafe {
        ptr::copy_nonoverlapping(
            old_virt.as_ptr::<u8>(),
            new_virt.as_mut_ptr::<u8>(),
            PAGE_SIZE_4KB as usize,
        );
    }

    let new_flags = PageFlags::USER_RW;

    // Drop the old mapping (decrements META_SLOTS for `old_phys`; if
    // this was the last reference the OSTD allocator path frees it,
    // otherwise other processes still hold their own mappings) and
    // install the freshly-allocated copy.
    let _ = ostd_unmap_4kb_user(vm_space, aligned_vaddr);
    if let Err(err) = ostd_map_4kb_user(vm_space, aligned_vaddr, new_phys, new_flags.bits()) {
        slopos_utils::klog_info!("cow::resolve_multi_ref: OSTD remap failed: {:?}", err);
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
