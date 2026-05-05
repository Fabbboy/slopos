use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::vm_space::VmSpace;

use crate::dual_paging::{ostd_map_4kb_user, ostd_resolve_cow_4kb, ostd_unmap_4kb_user};
use crate::error::MmError;
use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame, page_frame_get_ref};
use crate::paging::{
    ProcessPageDir, map_page_4kb_in_dir, paging_is_cow, paging_resolve_cow, virt_to_phys_in_dir,
};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::tlb;

pub fn handle_cow_fault(
    page_dir: *mut ProcessPageDir,
    vm_space: &mut KArc<VmSpace>,
    fault_addr: u64,
) -> Result<(), MmError> {
    if page_dir.is_null() {
        return Err(MmError::NullPageDir);
    }

    let vaddr = VirtAddr::new(fault_addr);
    let aligned_vaddr = VirtAddr::new(fault_addr & !(PAGE_SIZE_4KB - 1));

    if !paging_is_cow(page_dir, vaddr) {
        return Err(MmError::NotCowPage);
    }

    let old_phys = virt_to_phys_in_dir(page_dir, aligned_vaddr);
    if old_phys.is_null() {
        return Err(MmError::InvalidAddress);
    }

    let ref_count = page_frame_get_ref(old_phys);

    if ref_count <= 1 {
        return resolve_single_ref(page_dir, vm_space, aligned_vaddr);
    }

    resolve_multi_ref(page_dir, vm_space, aligned_vaddr, old_phys)
}

fn resolve_single_ref(
    page_dir: *mut ProcessPageDir,
    vm_space: &mut KArc<VmSpace>,
    aligned_vaddr: VirtAddr,
) -> Result<(), MmError> {
    // Sole owner: just flip PTE flags in-place (remove COW, add WRITABLE).
    // Do NOT use map_page_4kb_in_dir here — it would free the page we're remapping.
    if paging_resolve_cow(page_dir, aligned_vaddr) != 0 {
        return Err(MmError::MappingFailed);
    }
    // Dual-write: mirror the resolve into the OSTD VmSpace's parallel
    // PTE. ostd_resolve_cow_4kb is best-effort during the dual-write
    // window — the legacy half is the load-bearing CR3 source.
    let _ = ostd_resolve_cow_4kb(vm_space, aligned_vaddr);
    Ok(())
}

fn resolve_multi_ref(
    page_dir: *mut ProcessPageDir,
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

    // map_page_4kb_in_dir replaces the old PTE and frees old_phys (decrementing
    // its refcount). Do NOT call free_page_frame(old_phys) again — that would
    // double-decrement, freeing a page the other process still maps.
    if map_page_4kb_in_dir(page_dir, aligned_vaddr, new_phys, new_flags.bits()) != 0 {
        free_page_frame(new_phys);
        return Err(MmError::MappingFailed);
    }

    // Dual-write: replace the OSTD PTE too. Unmap first (the previous
    // wrap_static UFrame's static_borrowed=true Drop is a no-op so the
    // buddy-side accounting is unaffected) then re-map the fresh page.
    let _ = ostd_unmap_4kb_user(vm_space, aligned_vaddr);
    if let Err(err) = ostd_map_4kb_user(vm_space, aligned_vaddr, new_phys, new_flags.bits()) {
        slopos_utils::klog_info!("cow::resolve_multi_ref: OSTD remap failed: {:?}", err);
    }

    tlb::flush_page(aligned_vaddr);

    Ok(())
}

pub fn is_cow_fault(error_code: u64, page_dir: *mut ProcessPageDir, fault_addr: u64) -> bool {
    let is_write = (error_code & 0x02) != 0;
    let is_present = (error_code & 0x01) != 0;

    if !is_write || !is_present {
        return false;
    }

    paging_is_cow(page_dir, VirtAddr::new(fault_addr))
}
