use core::ffi::c_int;
use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::arch::x86_64::page_table::{PageTable, PageTableEntry, PageTableLevel};
use slopos_abi::arch::x86_64::paging::PageFlags;
use slopos_lib::{klog_debug, klog_info};

use super::walker::{PageTableWalker, WalkAction};
use crate::hhdm::{self, PhysAddrHhdm};
use crate::mm_constants::{
    KERNEL_PML4_INDEX, KERNEL_VIRTUAL_BASE, PAGE_SIZE_1GB, PAGE_SIZE_2MB, PAGE_SIZE_4KB,
};
use crate::page_alloc::{
    ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame, page_frame_can_free, page_frame_is_tracked,
};
use crate::tlb;

#[repr(C)]
pub struct ProcessPageDir {
    pub pml4: *mut PageTable,
    pub pml4_phys: PhysAddr,
    pub ref_count: u32,
    pub process_id: u32,
    pub next: *mut ProcessPageDir,
}

pub static mut EARLY_PML4: PageTable = PageTable::EMPTY;
pub static mut EARLY_PDPT: PageTable = PageTable::EMPTY;
pub static mut EARLY_PD: PageTable = PageTable::EMPTY;

static mut KERNEL_PAGE_DIR: ProcessPageDir = ProcessPageDir {
    pml4: unsafe { &mut EARLY_PML4 as *mut PageTable },
    pml4_phys: PhysAddr::NULL,
    ref_count: 1,
    process_id: 0,
    next: ptr::null_mut(),
};

static mut CURRENT_PAGE_DIR: *mut ProcessPageDir = unsafe { &mut KERNEL_PAGE_DIR };

fn table_empty(table: &PageTable) -> bool {
    table.iter().all(|e| !e.is_present())
}

fn alloc_page_table() -> Option<(PhysAddr, *mut PageTable)> {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        return None;
    }
    let virt = phys.to_virt().as_mut_ptr::<PageTable>();
    if virt.is_null() {
        free_page_frame(phys);
        return None;
    }
    unsafe { (*virt).zero() };
    Some((phys, virt))
}

fn intermediate_flags(user_mapping: bool) -> PageFlags {
    let base = PageFlags::PRESENT | PageFlags::WRITABLE;
    if user_mapping {
        base | PageFlags::USER
    } else {
        base
    }
}

#[inline]
fn phys_to_table(phys: PhysAddr) -> *mut PageTable {
    phys.to_virt().as_mut_ptr()
}

fn is_user_address(vaddr: VirtAddr) -> bool {
    let raw = vaddr.as_u64();
    raw < KERNEL_VIRTUAL_BASE && raw >= crate::mm_constants::USER_SPACE_START_VA
}

#[inline(always)]
fn get_cr3() -> PhysAddr {
    let mut cr3: u64;
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, preserves_flags));
    }
    PhysAddr::new(cr3 & !0xFFF)
}

#[inline(always)]
fn set_cr3(pml4_phys: PhysAddr) {
    unsafe {
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) pml4_phys.as_u64(),
            options(nostack, preserves_flags)
        );
    }
}

pub fn paging_copy_kernel_mappings(dest_pml4: *mut PageTable) {
    if dest_pml4.is_null() {
        return;
    }
    unsafe {
        if KERNEL_PAGE_DIR.pml4.is_null() {
            klog_info!("paging_copy_kernel_mappings: Kernel PML4 unavailable");
            return;
        }
        for i in 0..512 {
            *(&mut *dest_pml4).entry_mut(i) = *(&*KERNEL_PAGE_DIR.pml4).entry(i);
        }
        for i in 0..256 {
            *(&mut *dest_pml4).entry_mut(i) = PageTableEntry::EMPTY;
        }
    }
}

fn virt_to_phys_for_dir(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> PhysAddr {
    if page_dir.is_null() {
        return PhysAddr::NULL;
    }
    unsafe {
        let pml4 = (*page_dir).pml4;
        if pml4.is_null() {
            return PhysAddr::NULL;
        }
        let walker = PageTableWalker::new();
        match walker.walk(&*pml4, vaddr) {
            Ok(result) => result.phys_addr,
            Err(_) => PhysAddr::NULL,
        }
    }
}

pub fn virt_to_phys_in_dir(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> PhysAddr {
    virt_to_phys_for_dir(page_dir, vaddr)
}

pub fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    unsafe { virt_to_phys_for_dir(CURRENT_PAGE_DIR, vaddr) }
}

pub fn virt_to_phys_process(vaddr: VirtAddr, page_dir: *mut ProcessPageDir) -> PhysAddr {
    if page_dir.is_null() {
        return PhysAddr::NULL;
    }
    unsafe {
        let saved = CURRENT_PAGE_DIR;
        CURRENT_PAGE_DIR = page_dir;
        let phys = virt_to_phys(vaddr);
        CURRENT_PAGE_DIR = saved;
        phys
    }
}

fn map_page_in_directory(
    page_dir: *mut ProcessPageDir,
    vaddr: VirtAddr,
    paddr: PhysAddr,
    flags: u64,
    page_size: u64,
) -> c_int {
    if page_dir.is_null() {
        return -1;
    }

    if !vaddr.is_aligned(page_size) || !paddr.is_aligned(page_size) {
        return -1;
    }

    let flags = PageFlags::from_bits_truncate(flags);
    let user_mapping = flags.contains(PageFlags::USER) && is_user_address(vaddr);
    let inter_flags = intermediate_flags(user_mapping);

    unsafe {
        let pml4 = (*page_dir).pml4;
        if pml4.is_null() {
            return -1;
        }

        let pml4_idx = PageTableLevel::Four.index_of(vaddr);
        let pdpt_idx = PageTableLevel::Three.index_of(vaddr);
        let pd_idx = PageTableLevel::Two.index_of(vaddr);
        let pt_idx = PageTableLevel::One.index_of(vaddr);

        let pml4_entry = (&mut *pml4).entry_mut(pml4_idx);
        let pdpt = if !pml4_entry.is_present() {
            let Some((phys, ptr)) = alloc_page_table() else {
                return -1;
            };
            pml4_entry.set(phys, inter_flags);
            ptr
        } else {
            if pml4_entry.is_huge() {
                return -1;
            }
            if user_mapping && !pml4_entry.is_user() {
                pml4_entry.add_flags(PageFlags::USER);
            }
            phys_to_table(pml4_entry.address())
        };

        let pdpt_entry = (&mut *pdpt).entry_mut(pdpt_idx);

        if page_size == PAGE_SIZE_1GB {
            if pdpt_entry.is_present() {
                return -1;
            }
            pdpt_entry.set(paddr, flags | PageFlags::PRESENT | PageFlags::HUGE);
            tlb::flush_page(vaddr);
            return 0;
        }

        let pd = if !pdpt_entry.is_present() {
            let Some((phys, ptr)) = alloc_page_table() else {
                return -1;
            };
            pdpt_entry.set(phys, inter_flags);
            ptr
        } else {
            if pdpt_entry.is_huge() {
                return -1;
            }
            if user_mapping && !pdpt_entry.is_user() {
                pdpt_entry.add_flags(PageFlags::USER);
            }
            phys_to_table(pdpt_entry.address())
        };

        let pd_entry = (&mut *pd).entry_mut(pd_idx);

        if page_size == PAGE_SIZE_2MB {
            if pd_entry.is_present() {
                return -1;
            }
            pd_entry.set(paddr, flags | PageFlags::PRESENT | PageFlags::HUGE);
            tlb::flush_page(vaddr);
            return 0;
        }

        let pt = if !pd_entry.is_present() {
            let Some((phys, ptr)) = alloc_page_table() else {
                return -1;
            };
            pd_entry.set(phys, inter_flags);
            ptr
        } else {
            if pd_entry.is_huge() {
                return -1;
            }
            if user_mapping && !pd_entry.is_user() {
                pd_entry.add_flags(PageFlags::USER);
            }
            phys_to_table(pd_entry.address())
        };

        let pt_entry = (&mut *pt).entry_mut(pt_idx);

        if pt_entry.is_present() {
            let old_phys = pt_entry.address();
            if !old_phys.is_null() && page_frame_can_free(old_phys) != 0 {
                free_page_frame(old_phys);
            }
        }

        pt_entry.set(paddr, flags | PageFlags::PRESENT);
        tlb::flush_page(vaddr);
    }
    0
}

pub fn map_page_4kb_in_dir(
    page_dir: *mut ProcessPageDir,
    vaddr: VirtAddr,
    paddr: PhysAddr,
    flags: u64,
) -> c_int {
    map_page_in_directory(page_dir, vaddr, paddr, flags, PAGE_SIZE_4KB)
}

pub fn map_page_4kb(vaddr: VirtAddr, paddr: PhysAddr, flags: u64) -> c_int {
    unsafe { map_page_in_directory(CURRENT_PAGE_DIR, vaddr, paddr, flags, PAGE_SIZE_4KB) }
}

pub fn map_page_2mb(vaddr: VirtAddr, paddr: PhysAddr, flags: u64) -> c_int {
    unsafe { map_page_in_directory(CURRENT_PAGE_DIR, vaddr, paddr, flags, PAGE_SIZE_2MB) }
}

pub fn paging_map_shared_kernel_page(
    page_dir: *mut ProcessPageDir,
    kernel_vaddr: VirtAddr,
    user_vaddr: VirtAddr,
    flags: u64,
) -> c_int {
    if page_dir.is_null() {
        return -1;
    }
    if !is_user_address(user_vaddr) {
        return -1;
    }
    if !user_vaddr.is_aligned(PAGE_SIZE_4KB) || !kernel_vaddr.is_aligned(PAGE_SIZE_4KB) {
        return -1;
    }

    let phys = virt_to_phys_in_dir(unsafe { &mut KERNEL_PAGE_DIR }, kernel_vaddr);
    if phys.is_null() {
        return -1;
    }
    map_page_4kb_in_dir(page_dir, user_vaddr, phys, flags | PageFlags::USER.bits())
}

fn unmap_page_in_directory(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> c_int {
    if page_dir.is_null() {
        return -1;
    }
    unsafe {
        let pml4 = (*page_dir).pml4;
        if pml4.is_null() {
            return -1;
        }

        let pml4_idx = PageTableLevel::Four.index_of(vaddr);
        let pdpt_idx = PageTableLevel::Three.index_of(vaddr);
        let pd_idx = PageTableLevel::Two.index_of(vaddr);
        let pt_idx = PageTableLevel::One.index_of(vaddr);

        let pml4_entry = (&mut *pml4).entry_mut(pml4_idx);
        if !pml4_entry.is_present() {
            return 0;
        }
        let pml4_entry_phys = pml4_entry.address();

        let pdpt = phys_to_table(pml4_entry_phys);
        let pdpt_entry = (&mut *pdpt).entry_mut(pdpt_idx);
        if !pdpt_entry.is_present() {
            return 0;
        }

        if pdpt_entry.is_huge() {
            let phys = pdpt_entry.address();
            pdpt_entry.clear();
            if page_frame_can_free(phys) != 0 {
                free_page_frame(phys);
            }
            tlb::flush_page(vaddr);
            if table_empty(&*pdpt) {
                pml4_entry.clear();
                if page_frame_can_free(pml4_entry_phys) != 0 {
                    free_page_frame(pml4_entry_phys);
                }
            }
            return 0;
        }

        let pdpt_entry_phys = pdpt_entry.address();
        let pd = phys_to_table(pdpt_entry_phys);
        let pd_entry = (&mut *pd).entry_mut(pd_idx);
        if !pd_entry.is_present() {
            return 0;
        }

        if pd_entry.is_huge() {
            let phys = pd_entry.address();
            pd_entry.clear();
            if page_frame_can_free(phys) != 0 {
                free_page_frame(phys);
            }
            tlb::flush_page(vaddr);
        } else {
            let pd_entry_phys = pd_entry.address();
            let pt = phys_to_table(pd_entry_phys);
            if pt.is_null() {
                return -1;
            }
            let pt_entry = (&mut *pt).entry_mut(pt_idx);
            if pt_entry.is_present() {
                let phys = pt_entry.address();
                pt_entry.clear();
                tlb::flush_page(vaddr);
                if page_frame_can_free(phys) != 0 {
                    free_page_frame(phys);
                }
            }
            if table_empty(&*pt) {
                pd_entry.clear();
                if page_frame_can_free(pd_entry_phys) != 0 {
                    free_page_frame(pd_entry_phys);
                }
            }
        }

        if table_empty(&*pd) {
            pdpt_entry.clear();
            if page_frame_can_free(pdpt_entry_phys) != 0 {
                free_page_frame(pdpt_entry_phys);
            }
        }

        if table_empty(&*pdpt) {
            pml4_entry.clear();
            if page_frame_can_free(pml4_entry_phys) != 0 {
                free_page_frame(pml4_entry_phys);
            }
        }
    }

    0
}

pub fn unmap_page_in_dir(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> c_int {
    unmap_page_in_directory(page_dir, vaddr)
}

pub fn unmap_page(vaddr: VirtAddr) -> c_int {
    unsafe { unmap_page_in_directory(CURRENT_PAGE_DIR, vaddr) }
}

pub fn switch_page_directory(page_dir: *mut ProcessPageDir) -> c_int {
    if page_dir.is_null() {
        return -1;
    }
    unsafe {
        set_cr3((*page_dir).pml4_phys);
        CURRENT_PAGE_DIR = page_dir;
    }
    0
}

pub fn get_current_page_directory() -> *mut ProcessPageDir {
    unsafe { CURRENT_PAGE_DIR }
}

pub fn paging_set_current_directory(page_dir: *mut ProcessPageDir) {
    if !page_dir.is_null() {
        unsafe {
            CURRENT_PAGE_DIR = page_dir;
        }
    }
}

pub fn paging_get_kernel_directory() -> *mut ProcessPageDir {
    unsafe { &mut KERNEL_PAGE_DIR }
}

unsafe fn free_pt_level(pt: *mut PageTable, pt_phys: PhysAddr) {
    if pt.is_null() {
        return;
    }
    for entry in (*pt).iter() {
        if entry.is_present() {
            let phys = entry.address();
            if page_frame_can_free(phys) != 0 {
                free_page_frame(phys);
            }
        }
    }
    if page_frame_can_free(pt_phys) != 0 {
        free_page_frame(pt_phys);
    }
}

unsafe fn free_pd_level(pd: *mut PageTable, pd_phys: PhysAddr) {
    if pd.is_null() {
        return;
    }
    for entry in (*pd).iter() {
        if !entry.is_present() {
            continue;
        }
        let phys = entry.address();
        if entry.is_huge() {
            if page_frame_is_tracked(phys) != 0 {
                free_page_frame(phys);
            }
        } else {
            let pt = phys_to_table(phys);
            free_pt_level(pt, phys);
        }
    }
    if page_frame_can_free(pd_phys) != 0 {
        free_page_frame(pd_phys);
    }
}

unsafe fn free_pdpt_level(pdpt: *mut PageTable, pdpt_phys: PhysAddr) {
    if pdpt.is_null() {
        return;
    }
    for entry in (*pdpt).iter() {
        if !entry.is_present() {
            continue;
        }
        let phys = entry.address();
        if entry.is_huge() {
            if page_frame_is_tracked(phys) != 0 {
                free_page_frame(phys);
            }
        } else {
            let pd = phys_to_table(phys);
            free_pd_level(pd, phys);
        }
    }
    if page_frame_can_free(pdpt_phys) != 0 {
        free_page_frame(pdpt_phys);
    }
}

fn free_page_table_tree(page_dir: *mut ProcessPageDir) {
    if page_dir.is_null() {
        return;
    }
    unsafe {
        let pml4 = (*page_dir).pml4;
        if pml4.is_null() {
            return;
        }
        for pml4_idx in 0..KERNEL_PML4_INDEX {
            let entry = (&mut *pml4).entry_mut(pml4_idx);
            if !entry.is_present() {
                continue;
            }
            let pdpt_phys = entry.address();
            let pdpt = phys_to_table(pdpt_phys);
            free_pdpt_level(pdpt, pdpt_phys);
            entry.clear();
        }
    }
}

pub fn paging_free_user_space(page_dir: *mut ProcessPageDir) {
    free_page_table_tree(page_dir);
}

pub fn init_paging() {
    unsafe {
        let cr3 = get_cr3();
        KERNEL_PAGE_DIR.pml4_phys = cr3;

        let pml4_ptr = phys_to_table(KERNEL_PAGE_DIR.pml4_phys);
        if pml4_ptr.is_null() {
            panic!("Failed to translate kernel PML4 physical address");
        }
        KERNEL_PAGE_DIR.pml4 = pml4_ptr;

        let kernel_phys = virt_to_phys(VirtAddr::new(KERNEL_VIRTUAL_BASE));
        if kernel_phys.is_null() {
            panic!("Higher-half kernel mapping not found");
        }

        klog_debug!(
            "Higher-half kernel mapping verified at 0x{:x}",
            kernel_phys.as_u64()
        );

        let identity_phys = virt_to_phys(VirtAddr::new(0x100000));
        if identity_phys == PhysAddr::new(0x100000) || hhdm::is_available() {
            klog_debug!("Identity mapping verified");
        } else {
            klog_debug!("Identity mapping not found (may be normal after early boot)");
        }

        klog_debug!("Paging system initialized successfully");
    }
}

pub fn get_memory_layout_info(kernel_virt_base: *mut u64, kernel_phys_base: *mut u64) {
    unsafe {
        if !kernel_virt_base.is_null() {
            *kernel_virt_base = KERNEL_VIRTUAL_BASE;
        }
        if !kernel_phys_base.is_null() {
            *kernel_phys_base = virt_to_phys(VirtAddr::new(KERNEL_VIRTUAL_BASE)).as_u64();
        }
    }
}

pub fn is_mapped(vaddr: VirtAddr) -> c_int {
    (!virt_to_phys(vaddr).is_null()) as c_int
}

pub fn get_page_size(vaddr: VirtAddr) -> u64 {
    unsafe {
        if CURRENT_PAGE_DIR.is_null() || (*CURRENT_PAGE_DIR).pml4.is_null() {
            return 0;
        }
        let pml4 = (*CURRENT_PAGE_DIR).pml4;
        let walker = PageTableWalker::new();
        match walker.walk(&*pml4, vaddr) {
            Ok(result) => result.page_size,
            Err(_) => 0,
        }
    }
}

pub fn paging_mark_range_user(
    page_dir: *mut ProcessPageDir,
    start: VirtAddr,
    end: VirtAddr,
    writable: c_int,
) -> c_int {
    if page_dir.is_null() || unsafe { (*page_dir).pml4.is_null() } || start.as_u64() >= end.as_u64()
    {
        return -1;
    }

    let mut addr = start.as_u64() & !(PAGE_SIZE_4KB - 1);
    unsafe {
        while addr < end.as_u64() {
            let vaddr = VirtAddr::new(addr);

            let pml4_idx = PageTableLevel::Four.index_of(vaddr);
            let pdpt_idx = PageTableLevel::Three.index_of(vaddr);
            let pd_idx = PageTableLevel::Two.index_of(vaddr);
            let pt_idx = PageTableLevel::One.index_of(vaddr);

            let pml4_entry = (&mut *(*page_dir).pml4).entry_mut(pml4_idx);
            if !pml4_entry.is_present() {
                return -1;
            }
            if !pml4_entry.is_user() {
                pml4_entry.add_flags(PageFlags::USER);
            }

            let pdpt = phys_to_table(pml4_entry.address());
            if pdpt.is_null() {
                return -1;
            }
            let pdpt_entry = (&mut *pdpt).entry_mut(pdpt_idx);
            if !pdpt_entry.is_present() {
                return -1;
            }

            if pdpt_entry.is_huge() {
                pdpt_entry.add_flags(PageFlags::USER);
                if writable == 0 {
                    pdpt_entry.remove_flags(PageFlags::WRITABLE);
                } else {
                    pdpt_entry.add_flags(PageFlags::WRITABLE);
                }
                addr += PAGE_SIZE_1GB;
                continue;
            }

            let pd = phys_to_table(pdpt_entry.address());
            if pd.is_null() {
                return -1;
            }
            let pd_entry = (&mut *pd).entry_mut(pd_idx);
            if !pd_entry.is_present() {
                return -1;
            }

            if pd_entry.is_huge() {
                pd_entry.add_flags(PageFlags::USER);
                if writable == 0 {
                    pd_entry.remove_flags(PageFlags::WRITABLE);
                } else {
                    pd_entry.add_flags(PageFlags::WRITABLE);
                }
                addr += PAGE_SIZE_2MB;
                continue;
            }

            let pt = phys_to_table(pd_entry.address());
            if pt.is_null() {
                return -1;
            }
            let pt_entry = (&mut *pt).entry_mut(pt_idx);
            if !pt_entry.is_present() {
                return -1;
            }

            pt_entry.add_flags(PageFlags::USER);
            if writable == 0 {
                pt_entry.remove_flags(PageFlags::WRITABLE);
            } else {
                pt_entry.add_flags(PageFlags::WRITABLE);
            }
            addr += PAGE_SIZE_4KB;
        }
    }
    0
}

pub fn paging_is_user_accessible(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> c_int {
    if page_dir.is_null() || unsafe { (*page_dir).pml4.is_null() } {
        return 0;
    }
    unsafe {
        let pml4 = (*page_dir).pml4;
        let walker = PageTableWalker::new();

        let mut all_user = true;
        let result = walker.walk_with(&*pml4, vaddr, |_level, entry| {
            if entry.is_present() && !entry.is_user() {
                all_user = false;
            }
            WalkAction::Descend
        });

        (result.is_ok() && all_user) as c_int
    }
}
