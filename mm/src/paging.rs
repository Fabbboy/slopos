use core::ffi::c_int;
use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_lib::{klog_debug, klog_info};

use crate::hhdm::{self, PhysAddrHhdm};
use crate::mm_constants::{
    ENTRIES_PER_PAGE_TABLE, KERNEL_PML4_INDEX, KERNEL_VIRTUAL_BASE, PAGE_SIZE_1GB, PAGE_SIZE_2MB,
    PAGE_SIZE_4KB, PageFlags,
};
use crate::page_alloc::{
    ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame, page_frame_can_free, page_frame_is_tracked,
};

#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [u64; ENTRIES_PER_PAGE_TABLE],
}

impl PageTable {
    const fn zeroed() -> Self {
        Self {
            entries: [0; ENTRIES_PER_PAGE_TABLE],
        }
    }
}

#[repr(C)]
pub struct ProcessPageDir {
    pub pml4: *mut PageTable,
    pub pml4_phys: PhysAddr,
    pub ref_count: u32,
    pub process_id: u32,
    pub next: *mut ProcessPageDir,
}
pub static mut EARLY_PML4: PageTable = PageTable::zeroed();
pub static mut EARLY_PDPT: PageTable = PageTable::zeroed();
pub static mut EARLY_PD: PageTable = PageTable::zeroed();

static mut KERNEL_PAGE_DIR: ProcessPageDir = ProcessPageDir {
    pml4: unsafe { &mut EARLY_PML4 },
    pml4_phys: PhysAddr::NULL,
    ref_count: 1,
    process_id: 0,
    next: ptr::null_mut(),
};

static mut CURRENT_PAGE_DIR: *mut ProcessPageDir = unsafe { &mut KERNEL_PAGE_DIR };

const PTE_ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

fn table_empty(table: *const PageTable) -> bool {
    if table.is_null() {
        return true;
    }
    unsafe {
        for entry in (*table).entries.iter() {
            if entry & PageFlags::PRESENT.bits() != 0 {
                return false;
            }
        }
    }
    true
}

fn alloc_page_table(phys_out: &mut PhysAddr) -> *mut PageTable {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        return ptr::null_mut();
    }
    let virt = phys.to_virt().as_mut_ptr::<PageTable>();
    if virt.is_null() {
        free_page_frame(phys);
        return ptr::null_mut();
    }
    unsafe {
        (*virt).entries.fill(0);
    }
    *phys_out = phys;
    virt
}

fn intermediate_flags(user_mapping: bool) -> u64 {
    let base = PageFlags::PRESENT | PageFlags::WRITABLE;
    if user_mapping {
        (base | PageFlags::USER).bits()
    } else {
        base.bits()
    }
}

/// Convert a physical page table address to a virtual pointer.
#[inline]
fn phys_to_table(phys: PhysAddr) -> *mut PageTable {
    phys.to_virt().as_mut_ptr()
}

fn pml4_index(vaddr: VirtAddr) -> usize {
    ((vaddr.as_u64() >> 39) & 0x1FF) as usize
}
fn pdpt_index(vaddr: VirtAddr) -> usize {
    ((vaddr.as_u64() >> 30) & 0x1FF) as usize
}
fn pd_index(vaddr: VirtAddr) -> usize {
    ((vaddr.as_u64() >> 21) & 0x1FF) as usize
}
fn pt_index(vaddr: VirtAddr) -> usize {
    ((vaddr.as_u64() >> 12) & 0x1FF) as usize
}

fn pte_address(pte: u64) -> PhysAddr {
    PhysAddr::new(pte & PTE_ADDRESS_MASK)
}

fn pte_present(pte: u64) -> bool {
    pte & PageFlags::PRESENT.bits() != 0
}

fn pte_huge(pte: u64) -> bool {
    pte & PageFlags::HUGE.bits() != 0
}

fn pte_user(pte: u64) -> bool {
    pte & PageFlags::USER.bits() != 0
}

fn is_user_address(vaddr: VirtAddr) -> bool {
    let raw = vaddr.as_u64();
    raw < KERNEL_VIRTUAL_BASE && raw >= crate::mm_constants::USER_SPACE_START_VA
}

#[inline(always)]
fn invlpg(vaddr: VirtAddr) {
    unsafe {
        core::arch::asm!(
            "invlpg [{}]",
            in(reg) vaddr.as_u64(),
            options(nostack, preserves_flags)
        );
    }
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
        for i in 0..ENTRIES_PER_PAGE_TABLE {
            (*dest_pml4).entries[i] = (*KERNEL_PAGE_DIR.pml4).entries[i];
        }
        for i in 0..(ENTRIES_PER_PAGE_TABLE / 2) {
            (*dest_pml4).entries[i] = 0;
        }
    }
}

fn virt_to_phys_for_dir(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> PhysAddr {
    if page_dir.is_null() {
        klog_info!("virt_to_phys: No page directory");
        return PhysAddr::NULL;
    }
    unsafe {
        let pml4 = (*page_dir).pml4;
        if pml4.is_null() {
            return PhysAddr::NULL;
        }
        let pml4_entry = (*pml4).entries[pml4_index(vaddr)];
        if !pte_present(pml4_entry) {
            return PhysAddr::NULL;
        }
        let pdpt = phys_to_table(pte_address(pml4_entry));
        if pdpt.is_null() {
            klog_info!("virt_to_phys: Invalid PDPT address");
            return PhysAddr::NULL;
        }
        let pdpt_entry = (*pdpt).entries[pdpt_index(vaddr)];
        if !pte_present(pdpt_entry) {
            return PhysAddr::NULL;
        }
        if pte_huge(pdpt_entry) {
            let page_offset = vaddr.as_u64() & (PAGE_SIZE_1GB - 1);
            return pte_address(pdpt_entry).offset(page_offset);
        }

        let pd = phys_to_table(pte_address(pdpt_entry));
        if pd.is_null() {
            klog_info!("virt_to_phys: Invalid PD address");
            return PhysAddr::NULL;
        }
        let pd_entry = (*pd).entries[pd_index(vaddr)];
        if !pte_present(pd_entry) {
            return PhysAddr::NULL;
        }
        if pte_huge(pd_entry) {
            let page_offset = vaddr.as_u64() & (PAGE_SIZE_2MB - 1);
            return pte_address(pd_entry).offset(page_offset);
        }

        let pt = phys_to_table(pte_address(pd_entry));
        if pt.is_null() {
            klog_info!("virt_to_phys: Invalid PT address");
            return PhysAddr::NULL;
        }
        let pt_entry = (*pt).entries[pt_index(vaddr)];
        if !pte_present(pt_entry) {
            return PhysAddr::NULL;
        }
        let page_offset = vaddr.as_u64() & (PAGE_SIZE_4KB - 1);
        pte_address(pt_entry).offset(page_offset)
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
        klog_info!("map_page: No page directory provided");
        return -1;
    }

    if (vaddr.as_u64() & (page_size - 1)) != 0 || (paddr.as_u64() & (page_size - 1)) != 0 {
        klog_info!("map_page: Addresses not aligned to requested size");
        return -1;
    }

    let user_mapping = (flags & PageFlags::USER.bits() != 0) && is_user_address(vaddr);
    let inter_flags = intermediate_flags(user_mapping);

    unsafe {
        let pml4 = (*page_dir).pml4;
        if pml4.is_null() {
            return -1;
        }
        let pml4_idx = pml4_index(vaddr);
        let pdpt_idx = pdpt_index(vaddr);
        let pd_idx = pd_index(vaddr);
        let pt_idx = pt_index(vaddr);

        let pdpt: *mut PageTable;
        let mut pdpt_phys = PhysAddr::NULL;
        let pml4_entry = (*pml4).entries[pml4_idx];
        if !pte_present(pml4_entry) {
            pdpt = alloc_page_table(&mut pdpt_phys);
            if pdpt.is_null() {
                klog_info!("map_page: Failed to allocate PDPT");
                return -1;
            }
            (*pml4).entries[pml4_idx] = pdpt_phys.as_u64() | inter_flags;
        } else {
            if pte_huge(pml4_entry) {
                klog_info!("map_page: PML4 entry is huge (unexpected)");
                return -1;
            }
            pdpt_phys = pte_address(pml4_entry);
            pdpt = phys_to_table(pdpt_phys);
            if user_mapping && (pml4_entry & PageFlags::USER.bits()) == 0 {
                (*pml4).entries[pml4_idx] = (pml4_entry & !0xFFF) | inter_flags;
            }
        }

        let pd: *mut PageTable;
        let mut pd_phys = PhysAddr::NULL;
        let pdpt_entry = (*pdpt).entries[pdpt_idx];

        if page_size == PAGE_SIZE_1GB {
            if pte_present(pdpt_entry) {
                klog_info!("map_page: PDPT entry already present for 1GB mapping");
                return -1;
            }
            (*pdpt).entries[pdpt_idx] =
                paddr.as_u64() | flags | PageFlags::HUGE.bits() | PageFlags::PRESENT.bits();
            invlpg(vaddr);
            return 0;
        }

        if !pte_present(pdpt_entry) {
            pd = alloc_page_table(&mut pd_phys);
            if pd.is_null() {
                klog_info!("map_page: Failed to allocate PD");
                return -1;
            }
            (*pdpt).entries[pdpt_idx] = pd_phys.as_u64() | inter_flags;
        } else {
            if pte_huge(pdpt_entry) {
                klog_info!("map_page: PDPT entry is a huge page");
                return -1;
            }
            pd_phys = pte_address(pdpt_entry);
            pd = phys_to_table(pd_phys);
            if user_mapping && (pdpt_entry & PageFlags::USER.bits()) == 0 {
                (*pdpt).entries[pdpt_idx] = (pdpt_entry & !0xFFF) | inter_flags;
            }
        }

        let pd_entry = (*pd).entries[pd_idx];
        if page_size == PAGE_SIZE_2MB {
            if pte_present(pd_entry) {
                klog_info!("map_page: PD entry already present for 2MB mapping");
                return -1;
            }
            (*pd).entries[pd_idx] =
                paddr.as_u64() | flags | PageFlags::HUGE.bits() | PageFlags::PRESENT.bits();
            invlpg(vaddr);
            return 0;
        }

        let mut pt_entry = pd_entry;
        if !pte_present(pd_entry) {
            let mut pt_phys = PhysAddr::NULL;
            let pt = alloc_page_table(&mut pt_phys);
            if pt.is_null() {
                klog_info!("map_page: Failed to allocate PT");
                return -1;
            }
            (*pd).entries[pd_idx] = pt_phys.as_u64() | inter_flags;
            pt_entry = (*pd).entries[pd_idx];
        }

        if pte_huge(pd_entry) {
            klog_info!("map_page: PD entry is a large page");
            return -1;
        }

        let pt = phys_to_table(pte_address(pt_entry));
        if pt.is_null() {
            klog_info!("map_page: Invalid PT pointer");
            return -1;
        }

        if (*pt).entries[pt_idx] & PageFlags::PRESENT.bits() != 0 {
            klog_info!(
                "map_page: Virtual address 0x{:x} already mapped (entry=0x{:x})",
                vaddr.as_u64(),
                (*pt).entries[pt_idx]
            );
            // For ELF loading, unmap the existing page first
            let old_phys = (*pt).entries[pt_idx] & 0x000f_ffff_ffff_f000;
            let old_phys = PhysAddr::new(old_phys);
            if !old_phys.is_null() && page_frame_can_free(old_phys) != 0 {
                free_page_frame(old_phys);
            }
            // Continue to overwrite the mapping
        }

        if user_mapping && (pt_entry & PageFlags::USER.bits()) == 0 {
            (*pd).entries[pd_idx] = (pt_entry & !0xFFF) | inter_flags;
        }

        (*pt).entries[pt_idx] = paddr.as_u64() | (flags | PageFlags::PRESENT.bits());
        invlpg(vaddr);
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
    if (user_vaddr.as_u64() & (PAGE_SIZE_4KB - 1)) != 0
        || (kernel_vaddr.as_u64() & (PAGE_SIZE_4KB - 1)) != 0
    {
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
        klog_info!("unmap_page: No page directory provided");
        return -1;
    }
    unsafe {
        let pml4 = (*page_dir).pml4;
        if pml4.is_null() {
            return -1;
        }
        let pml4_idx = pml4_index(vaddr);
        let pdpt_idx = pdpt_index(vaddr);
        let pd_idx = pd_index(vaddr);
        let pt_idx = pt_index(vaddr);

        let pml4_entry = (*pml4).entries[pml4_idx];
        if !pte_present(pml4_entry) {
            return 0;
        }

        let pdpt = phys_to_table(pte_address(pml4_entry));
        let pdpt_entry = (*pdpt).entries[pdpt_idx];
        if !pte_present(pdpt_entry) {
            return 0;
        }

        if pte_huge(pdpt_entry) {
            let phys = pte_address(pdpt_entry);
            (*pdpt).entries[pdpt_idx] = 0;
            if page_frame_can_free(phys) != 0 {
                free_page_frame(phys);
            }
            invlpg(vaddr);
            if table_empty(pdpt as *const PageTable) {
                (*pml4).entries[pml4_idx] = 0;
                let pml4_phys = pte_address(pml4_entry);
                if page_frame_can_free(pml4_phys) != 0 {
                    free_page_frame(pml4_phys);
                }
            }
            return 0;
        }

        let pd = phys_to_table(pte_address(pdpt_entry));
        let pd_entry = (*pd).entries[pd_idx];
        if !pte_present(pd_entry) {
            return 0;
        }

        if pte_huge(pd_entry) {
            let phys = pte_address(pd_entry);
            (*pd).entries[pd_idx] = 0;
            if page_frame_can_free(phys) != 0 {
                free_page_frame(phys);
            }
            invlpg(vaddr);
        } else {
            let pt = phys_to_table(pte_address(pd_entry));
            if pt.is_null() {
                return -1;
            }
            if (*pt).entries[pt_idx] & PageFlags::PRESENT.bits() != 0 {
                let phys = pte_address((*pt).entries[pt_idx]);
                (*pt).entries[pt_idx] = 0;
                invlpg(vaddr);
                if page_frame_can_free(phys) != 0 {
                    free_page_frame(phys);
                }
            }
            if table_empty(pt as *const PageTable) {
                let phys_pt = pte_address(pd_entry);
                (*pd).entries[pd_idx] = 0;
                if page_frame_can_free(phys_pt) != 0 {
                    free_page_frame(phys_pt);
                }
            }
        }

        if table_empty(pd as *const PageTable) {
            let phys_pd = pte_address(pdpt_entry);
            (*pdpt).entries[pdpt_idx] = 0;
            if page_frame_can_free(phys_pd) != 0 {
                free_page_frame(phys_pd);
            }
        }

        if table_empty(pdpt as *const PageTable) {
            (*pml4).entries[pml4_idx] = 0;
            let pml4_phys = pte_address(pml4_entry);
            if page_frame_can_free(pml4_phys) != 0 {
                free_page_frame(pml4_phys);
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
        klog_info!("switch_page_directory: Invalid page directory");
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

/// Free all 4KB page frames in a page table, then free the PT itself
unsafe fn free_pt_level(pt: *mut PageTable, pt_phys: PhysAddr) {
    if pt.is_null() {
        return;
    }
    for i in 0..ENTRIES_PER_PAGE_TABLE {
        let pte = (*pt).entries[i];
        if pte_present(pte) {
            let phys = pte_address(pte);
            if page_frame_can_free(phys) != 0 {
                free_page_frame(phys);
            }
        }
    }
    if page_frame_can_free(pt_phys) != 0 {
        free_page_frame(pt_phys);
    }
}

/// Free all entries in a page directory (2MB huge or PT subtrees), then free PD itself
unsafe fn free_pd_level(pd: *mut PageTable, pd_phys: PhysAddr) {
    if pd.is_null() {
        return;
    }
    for i in 0..ENTRIES_PER_PAGE_TABLE {
        let pde = (*pd).entries[i];
        if !pte_present(pde) {
            continue;
        }
        let phys = pte_address(pde);
        if pte_huge(pde) {
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

/// Free all entries in a PDPT (1GB huge or PD subtrees), then free PDPT itself
unsafe fn free_pdpt_level(pdpt: *mut PageTable, pdpt_phys: PhysAddr) {
    if pdpt.is_null() {
        return;
    }
    for i in 0..ENTRIES_PER_PAGE_TABLE {
        let pdpte = (*pdpt).entries[i];
        if !pte_present(pdpte) {
            continue;
        }
        let phys = pte_address(pdpte);
        if pte_huge(pdpte) {
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
            let pml4e = (*pml4).entries[pml4_idx];
            if !pte_present(pml4e) {
                continue;
            }
            let pdpt_phys = pte_address(pml4e);
            let pdpt = phys_to_table(pdpt_phys);
            free_pdpt_level(pdpt, pdpt_phys);
            (*pml4).entries[pml4_idx] = 0;
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
        let pml4_entry = (*(*CURRENT_PAGE_DIR).pml4).entries[pml4_index(vaddr)];
        if !pte_present(pml4_entry) {
            return 0;
        }
        let pdpt = phys_to_table(pte_address(pml4_entry));
        let pdpt_entry = (*pdpt).entries[pdpt_index(vaddr)];
        if !pte_present(pdpt_entry) {
            return 0;
        }
        if pte_huge(pdpt_entry) {
            return PAGE_SIZE_1GB;
        }
        let pd = phys_to_table(pte_address(pdpt_entry));
        let pd_entry = (*pd).entries[pd_index(vaddr)];
        if !pte_present(pd_entry) {
            return 0;
        }
        if pte_huge(pd_entry) {
            return PAGE_SIZE_2MB;
        }
        PAGE_SIZE_4KB
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
            let pml4e = &mut (*(*page_dir).pml4).entries[pml4_index(vaddr)];
            if !pte_present(*pml4e) {
                return -1;
            }
            if !pte_user(*pml4e) {
                *pml4e |= PageFlags::USER.bits();
            }
            let pdpt = phys_to_table(pte_address(*pml4e));
            if pdpt.is_null() {
                return -1;
            }
            let pdpte = &mut (*pdpt).entries[pdpt_index(vaddr)];
            if !pte_present(*pdpte) {
                return -1;
            }
            if pte_huge(*pdpte) {
                let mut flags = *pdpte | PageFlags::USER.bits();
                if writable == 0 {
                    flags &= !PageFlags::WRITABLE.bits();
                } else {
                    flags |= PageFlags::WRITABLE.bits();
                }
                *pdpte = flags;
                addr += PAGE_SIZE_1GB;
                continue;
            }
            let pd = phys_to_table(pte_address(*pdpte));
            if pd.is_null() {
                return -1;
            }
            let pde = &mut (*pd).entries[pd_index(vaddr)];
            if !pte_present(*pde) {
                return -1;
            }
            if pte_huge(*pde) {
                let mut flags = *pde | PageFlags::USER.bits();
                if writable == 0 {
                    flags &= !PageFlags::WRITABLE.bits();
                } else {
                    flags |= PageFlags::WRITABLE.bits();
                }
                *pde = flags;
                addr += PAGE_SIZE_2MB;
                continue;
            }
            let pt = phys_to_table(pte_address(*pde));
            if pt.is_null() {
                return -1;
            }
            let pte = &mut (*pt).entries[pt_index(vaddr)];
            if !pte_present(*pte) {
                return -1;
            }
            let mut flags = *pte | PageFlags::USER.bits();
            if writable == 0 {
                flags &= !PageFlags::WRITABLE.bits();
            } else {
                flags |= PageFlags::WRITABLE.bits();
            }
            *pte = flags;
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
        let pml4_entry = (*(*page_dir).pml4).entries[pml4_index(vaddr)];
        if !pte_present(pml4_entry) || !pte_user(pml4_entry) {
            return 0;
        }
        let pdpt = phys_to_table(pte_address(pml4_entry));
        if pdpt.is_null() {
            return 0;
        }
        let pdpt_entry = (*pdpt).entries[pdpt_index(vaddr)];
        if !pte_present(pdpt_entry) || !pte_user(pdpt_entry) {
            return 0;
        }
        if pte_huge(pdpt_entry) {
            return 1;
        }
        let pd = phys_to_table(pte_address(pdpt_entry));
        if pd.is_null() {
            return 0;
        }
        let pd_entry = (*pd).entries[pd_index(vaddr)];
        if !pte_present(pd_entry) || !pte_user(pd_entry) {
            return 0;
        }
        if pte_huge(pd_entry) {
            return 1;
        }
        let pt = phys_to_table(pte_address(pd_entry));
        if pt.is_null() {
            return 0;
        }
        let pt_entry = (*pt).entries[pt_index(vaddr)];
        (pte_present(pt_entry) && pte_user(pt_entry)) as c_int
    }
}
