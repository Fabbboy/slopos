#![allow(dead_code)]

use core::ffi::{c_char, c_int};
use core::ptr;

use crate::mm_constants::{
    ENTRIES_PER_PAGE_TABLE, HHDM_VIRT_BASE, KERNEL_PDPT_INDEX, KERNEL_PML4_INDEX, KERNEL_VIRTUAL_BASE,
    PAGE_KERNEL_RW, PAGE_PRESENT, PAGE_SIZE_1GB, PAGE_SIZE_2MB, PAGE_SIZE_4KB, PAGE_SIZE_FLAG_COMPAT,
    PAGE_USER, PAGE_WRITABLE,
};
use crate::page_alloc::{
    alloc_page_frame, free_page_frame, page_frame_can_free, page_frame_is_tracked, ALLOC_FLAG_ZERO,
};
use crate::phys_virt::mm_phys_to_virt;

extern "C" {
    fn klog_printf(level: slopos_lib::klog::KlogLevel, fmt: *const c_char, ...) -> c_int;
    fn kernel_panic(msg: *const c_char) -> !;
    fn is_hhdm_available() -> c_int;
}

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
    pub pml4_phys: u64,
    pub ref_count: u32,
    pub process_id: u32,
    pub next: *mut ProcessPageDir,
}

#[no_mangle]
pub static mut early_pml4: PageTable = PageTable::zeroed();
#[no_mangle]
pub static mut early_pdpt: PageTable = PageTable::zeroed();
#[no_mangle]
pub static mut early_pd: PageTable = PageTable::zeroed();

static mut KERNEL_PAGE_DIR: ProcessPageDir = ProcessPageDir {
    pml4: unsafe { &mut early_pml4 },
    pml4_phys: 0,
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
            if entry & PAGE_PRESENT != 0 {
                return false;
            }
        }
    }
    true
}

fn alloc_page_table(phys_out: &mut u64) -> *mut PageTable {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys == 0 {
        return ptr::null_mut();
    }
    let virt = mm_phys_to_virt(phys) as *mut PageTable;
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
    PAGE_PRESENT | PAGE_WRITABLE | if user_mapping { PAGE_USER } else { 0 }
}

fn pml4_index(vaddr: u64) -> usize {
    ((vaddr >> 39) & 0x1FF) as usize
}
fn pdpt_index(vaddr: u64) -> usize {
    ((vaddr >> 30) & 0x1FF) as usize
}
fn pd_index(vaddr: u64) -> usize {
    ((vaddr >> 21) & 0x1FF) as usize
}
fn pt_index(vaddr: u64) -> usize {
    ((vaddr >> 12) & 0x1FF) as usize
}

fn pte_address(pte: u64) -> u64 {
    pte & PTE_ADDRESS_MASK
}

fn pte_present(pte: u64) -> bool {
    pte & PAGE_PRESENT != 0
}

fn pte_huge(pte: u64) -> bool {
    pte & PAGE_SIZE_FLAG_COMPAT != 0
}

fn pte_user(pte: u64) -> bool {
    pte & PAGE_USER != 0
}

fn is_user_address(vaddr: u64) -> bool {
    vaddr < KERNEL_VIRTUAL_BASE && vaddr >= 0x0000_0000_0040_0000
}

#[inline(always)]
fn invlpg(vaddr: u64) {
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags));
    }
}

#[inline(always)]
fn get_cr3() -> u64 {
    let mut cr3: u64;
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, preserves_flags));
    }
    cr3
}

#[inline(always)]
fn set_cr3(pml4_phys: u64) {
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) pml4_phys, options(nostack, preserves_flags));
    }
}

#[no_mangle]
pub extern "C" fn paging_copy_kernel_mappings(dest_pml4: *mut PageTable) {
    if dest_pml4.is_null() {
        return;
    }
    unsafe {
        if KERNEL_PAGE_DIR.pml4.is_null() {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"paging_copy_kernel_mappings: Kernel PML4 unavailable\n\0".as_ptr() as *const c_char,
            );
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

fn virt_to_phys_for_dir(page_dir: *mut ProcessPageDir, vaddr: u64) -> u64 {
    if page_dir.is_null() {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"virt_to_phys: No page directory\n\0".as_ptr() as *const c_char,
            );
        }
        return 0;
    }
    unsafe {
        let pml4 = (*page_dir).pml4;
        if pml4.is_null() {
            return 0;
        }
        let pml4_entry = (*pml4).entries[pml4_index(vaddr)];
        if !pte_present(pml4_entry) {
            return 0;
        }
        let pdpt = mm_phys_to_virt(pte_address(pml4_entry)) as *mut PageTable;
        if pdpt.is_null() {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"virt_to_phys: Invalid PDPT address\n\0".as_ptr() as *const c_char,
            );
            return 0;
        }
        let pdpt_entry = (*pdpt).entries[pdpt_index(vaddr)];
        if !pte_present(pdpt_entry) {
            return 0;
        }
        if pte_huge(pdpt_entry) {
            let page_offset = vaddr & (PAGE_SIZE_1GB - 1);
            return pte_address(pdpt_entry) + page_offset;
        }

        let pd = mm_phys_to_virt(pte_address(pdpt_entry)) as *mut PageTable;
        if pd.is_null() {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"virt_to_phys: Invalid PD address\n\0".as_ptr() as *const c_char,
            );
            return 0;
        }
        let pd_entry = (*pd).entries[pd_index(vaddr)];
        if !pte_present(pd_entry) {
            return 0;
        }
        if pte_huge(pd_entry) {
            let page_offset = vaddr & (PAGE_SIZE_2MB - 1);
            return pte_address(pd_entry) + page_offset;
        }

        let pt = mm_phys_to_virt(pte_address(pd_entry)) as *mut PageTable;
        if pt.is_null() {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"virt_to_phys: Invalid PT address\n\0".as_ptr() as *const c_char,
            );
            return 0;
        }
        let pt_entry = (*pt).entries[pt_index(vaddr)];
        if !pte_present(pt_entry) {
            return 0;
        }
        let page_offset = vaddr & (PAGE_SIZE_4KB - 1);
        pte_address(pt_entry) + page_offset
    }
}

#[no_mangle]
pub extern "C" fn virt_to_phys_in_dir(page_dir: *mut ProcessPageDir, vaddr: u64) -> u64 {
    virt_to_phys_for_dir(page_dir, vaddr)
}

#[no_mangle]
pub extern "C" fn virt_to_phys(vaddr: u64) -> u64 {
    unsafe { virt_to_phys_for_dir(CURRENT_PAGE_DIR, vaddr) }
}

#[no_mangle]
pub extern "C" fn virt_to_phys_process(vaddr: u64, page_dir: *mut ProcessPageDir) -> u64 {
    if page_dir.is_null() {
        return 0;
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
    vaddr: u64,
    paddr: u64,
    flags: u64,
    page_size: u64,
) -> c_int {
    if page_dir.is_null() {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"map_page: No page directory provided\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    if (vaddr & (page_size - 1)) != 0 || (paddr & (page_size - 1)) != 0 {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"map_page: Addresses not aligned to requested size\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    let user_mapping = (flags & PAGE_USER != 0) && is_user_address(vaddr);
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

        let mut pdpt: *mut PageTable;
        let mut pdpt_phys = 0;
        let pml4_entry = (*pml4).entries[pml4_idx];
        if !pte_present(pml4_entry) {
            pdpt = alloc_page_table(&mut pdpt_phys);
            if pdpt.is_null() {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"map_page: Failed to allocate PDPT\n\0".as_ptr() as *const c_char,
                );
                return -1;
            }
            (*pml4).entries[pml4_idx] = pdpt_phys | inter_flags;
        } else {
            if pte_huge(pml4_entry) {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"map_page: PML4 entry is huge (unexpected)\n\0".as_ptr() as *const c_char,
                );
                return -1;
            }
            pdpt_phys = pte_address(pml4_entry);
            pdpt = mm_phys_to_virt(pdpt_phys) as *mut PageTable;
            if user_mapping && (pml4_entry & PAGE_USER) == 0 {
                (*pml4).entries[pml4_idx] = (pml4_entry & !0xFFF) | inter_flags;
            }
        }

        let mut pd: *mut PageTable;
        let mut pd_phys = 0;
        let pdpt_entry = (*pdpt).entries[pdpt_idx];

        if page_size == PAGE_SIZE_1GB {
            if pte_present(pdpt_entry) {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"map_page: PDPT entry already present for 1GB mapping\n\0".as_ptr() as *const c_char,
                );
                return -1;
            }
            (*pdpt).entries[pdpt_idx] = paddr | flags | PAGE_SIZE_FLAG_COMPAT | PAGE_PRESENT;
            invlpg(vaddr);
            return 0;
        }

        if !pte_present(pdpt_entry) {
            pd = alloc_page_table(&mut pd_phys);
            if pd.is_null() {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"map_page: Failed to allocate PD\n\0".as_ptr() as *const c_char,
                );
                return -1;
            }
            (*pdpt).entries[pdpt_idx] = pd_phys | inter_flags;
        } else {
            if pte_huge(pdpt_entry) {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"map_page: PDPT entry is a huge page\n\0".as_ptr() as *const c_char,
                );
                return -1;
            }
            pd_phys = pte_address(pdpt_entry);
            pd = mm_phys_to_virt(pd_phys) as *mut PageTable;
            if user_mapping && (pdpt_entry & PAGE_USER) == 0 {
                (*pdpt).entries[pdpt_idx] = (pdpt_entry & !0xFFF) | inter_flags;
            }
        }

        let pd_entry = (*pd).entries[pd_idx];
        if page_size == PAGE_SIZE_2MB {
            if pte_present(pd_entry) {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"map_page: PD entry already present for 2MB mapping\n\0".as_ptr() as *const c_char,
                );
                return -1;
            }
            (*pd).entries[pd_idx] = paddr | flags | PAGE_SIZE_FLAG_COMPAT | PAGE_PRESENT;
            invlpg(vaddr);
            return 0;
        }

        let mut pt_entry = pd_entry;
        if !pte_present(pd_entry) {
            let mut pt_phys = 0;
            let pt = alloc_page_table(&mut pt_phys);
            if pt.is_null() {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"map_page: Failed to allocate PT\n\0".as_ptr() as *const c_char,
                );
                return -1;
            }
            (*pd).entries[pd_idx] = pt_phys | inter_flags;
            pt_entry = (*pd).entries[pd_idx];
        }

        if pte_huge(pd_entry) {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"map_page: PD entry is a large page\n\0".as_ptr() as *const c_char,
            );
            return -1;
        }

        let pt = mm_phys_to_virt(pte_address(pt_entry)) as *mut PageTable;
        if pt.is_null() {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"map_page: Invalid PT pointer\n\0".as_ptr() as *const c_char,
            );
            return -1;
        }

        if (*pt).entries[pt_idx] & PAGE_PRESENT != 0 {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"map_page: Virtual address already mapped\n\0".as_ptr() as *const c_char,
            );
            return -1;
        }

        if user_mapping && (pt_entry & PAGE_USER) == 0 {
            (*pd).entries[pd_idx] = (pt_entry & !0xFFF) | inter_flags;
        }

        (*pt).entries[pt_idx] = paddr | (flags | PAGE_PRESENT);
        invlpg(vaddr);
    }
    0
}

#[no_mangle]
pub extern "C" fn map_page_4kb_in_dir(
    page_dir: *mut ProcessPageDir,
    vaddr: u64,
    paddr: u64,
    flags: u64,
) -> c_int {
    map_page_in_directory(page_dir, vaddr, paddr, flags, PAGE_SIZE_4KB)
}

#[no_mangle]
pub extern "C" fn map_page_4kb(vaddr: u64, paddr: u64, flags: u64) -> c_int {
    unsafe { map_page_in_directory(CURRENT_PAGE_DIR, vaddr, paddr, flags, PAGE_SIZE_4KB) }
}

#[no_mangle]
pub extern "C" fn map_page_2mb(vaddr: u64, paddr: u64, flags: u64) -> c_int {
    unsafe { map_page_in_directory(CURRENT_PAGE_DIR, vaddr, paddr, flags, PAGE_SIZE_2MB) }
}

#[no_mangle]
pub extern "C" fn paging_map_shared_kernel_page(
    page_dir: *mut ProcessPageDir,
    kernel_vaddr: u64,
    user_vaddr: u64,
    flags: u64,
) -> c_int {
    if page_dir.is_null() {
        return -1;
    }
    if !is_user_address(user_vaddr) {
        return -1;
    }
    if (user_vaddr & (PAGE_SIZE_4KB - 1)) != 0 || (kernel_vaddr & (PAGE_SIZE_4KB - 1)) != 0 {
        return -1;
    }

    let phys = virt_to_phys_in_dir(unsafe { &mut KERNEL_PAGE_DIR }, kernel_vaddr);
    if phys == 0 {
        return -1;
    }
    map_page_4kb_in_dir(page_dir, user_vaddr, phys, flags | PAGE_USER)
}

fn unmap_page_in_directory(page_dir: *mut ProcessPageDir, vaddr: u64) -> c_int {
    if page_dir.is_null() {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"unmap_page: No page directory provided\n\0".as_ptr() as *const c_char,
            );
        }
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

        let pdpt = mm_phys_to_virt(pte_address(pml4_entry)) as *mut PageTable;
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
                if page_frame_can_free(pte_address(pml4_entry)) != 0 {
                    free_page_frame(pte_address(pml4_entry));
                }
            }
            return 0;
        }

        let pd = mm_phys_to_virt(pte_address(pdpt_entry)) as *mut PageTable;
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
            let pt = mm_phys_to_virt(pte_address(pd_entry)) as *mut PageTable;
            if pt.is_null() {
                return -1;
            }
            if (*pt).entries[pt_idx] & PAGE_PRESENT != 0 {
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
            if page_frame_can_free(pte_address(pml4_entry)) != 0 {
                free_page_frame(pte_address(pml4_entry));
            }
        }
    }

    0
}

#[no_mangle]
pub extern "C" fn unmap_page_in_dir(page_dir: *mut ProcessPageDir, vaddr: u64) -> c_int {
    unmap_page_in_directory(page_dir, vaddr)
}

#[no_mangle]
pub extern "C" fn unmap_page(vaddr: u64) -> c_int {
    unsafe { unmap_page_in_directory(CURRENT_PAGE_DIR, vaddr) }
}

#[no_mangle]
pub extern "C" fn switch_page_directory(page_dir: *mut ProcessPageDir) -> c_int {
    if page_dir.is_null() {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"switch_page_directory: Invalid page directory\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }
    unsafe {
        set_cr3((*page_dir).pml4_phys);
        CURRENT_PAGE_DIR = page_dir;
        klog_printf(
            slopos_lib::klog::KlogLevel::Debug,
            b"Switched to process page directory\n\0".as_ptr() as *const c_char,
        );
    }
    0
}

#[no_mangle]
pub extern "C" fn get_current_page_directory() -> *mut ProcessPageDir {
    unsafe { CURRENT_PAGE_DIR }
}

#[no_mangle]
pub extern "C" fn paging_set_current_directory(page_dir: *mut ProcessPageDir) {
    if !page_dir.is_null() {
        unsafe {
            CURRENT_PAGE_DIR = page_dir;
        }
    }
}

#[no_mangle]
pub extern "C" fn paging_get_kernel_directory() -> *mut ProcessPageDir {
    unsafe { &mut KERNEL_PAGE_DIR }
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
            let pdpt = mm_phys_to_virt(pte_address(pml4e)) as *mut PageTable;
            for pdpt_idx in 0..ENTRIES_PER_PAGE_TABLE {
                let pdpte = (*pdpt).entries[pdpt_idx];
                if !pte_present(pdpte) {
                    continue;
                }
                if pte_huge(pdpte) {
                    let phys = pte_address(pdpte);
                    if page_frame_is_tracked(phys) != 0 {
                        free_page_frame(phys);
                    }
                    continue;
                }
                let pd = mm_phys_to_virt(pte_address(pdpte)) as *mut PageTable;
                for pd_idx in 0..ENTRIES_PER_PAGE_TABLE {
                    let pde = (*pd).entries[pd_idx];
                    if !pte_present(pde) {
                        continue;
                    }
                    if pte_huge(pde) {
                        let phys = pte_address(pde);
                        if page_frame_is_tracked(phys) != 0 {
                            free_page_frame(phys);
                        }
                        continue;
                    }
                    let pt = mm_phys_to_virt(pte_address(pde)) as *mut PageTable;
                    for pt_idx in 0..ENTRIES_PER_PAGE_TABLE {
                        let pte = (*pt).entries[pt_idx];
                        if pte_present(pte) {
                            let phys = pte_address(pte);
                            if page_frame_can_free(phys) != 0 {
                                free_page_frame(phys);
                            }
                        }
                    }
                    let phys_pd = pte_address(pde);
                    if page_frame_can_free(phys_pd) != 0 {
                        free_page_frame(phys_pd);
                    }
                }
                let phys_pdpt = pte_address(pdpte);
                if page_frame_can_free(phys_pdpt) != 0 {
                    free_page_frame(phys_pdpt);
                }
            }
            let phys_pml4e = pte_address(pml4e);
            if page_frame_can_free(phys_pml4e) != 0 {
                free_page_frame(phys_pml4e);
            }
            (*pml4).entries[pml4_idx] = 0;
        }
    }
}

#[no_mangle]
pub extern "C" fn paging_free_user_space(page_dir: *mut ProcessPageDir) {
    free_page_table_tree(page_dir);
}

#[no_mangle]
pub extern "C" fn init_paging() {
    unsafe {
        let cr3 = get_cr3();
        KERNEL_PAGE_DIR.pml4_phys = cr3 & !0xFFF;

        let pml4_ptr = mm_phys_to_virt(KERNEL_PAGE_DIR.pml4_phys) as *mut PageTable;
        if pml4_ptr.is_null() {
            kernel_panic(b"Failed to translate kernel PML4 physical address\0".as_ptr() as *const c_char);
        }
        KERNEL_PAGE_DIR.pml4 = pml4_ptr;

        let kernel_phys = virt_to_phys(KERNEL_VIRTUAL_BASE);
        if kernel_phys == 0 {
            kernel_panic(b"Higher-half kernel mapping not found\0".as_ptr() as *const c_char);
        }

        klog_printf(
            slopos_lib::klog::KlogLevel::Debug,
            b"Higher-half kernel mapping verified at 0x%llx\n\0".as_ptr() as *const c_char,
            kernel_phys,
        );

        let identity_phys = virt_to_phys(0x100000);
        if identity_phys == 0x100000 || is_hhdm_available() != 0 {
            klog_printf(
                slopos_lib::klog::KlogLevel::Debug,
                b"Identity mapping verified\n\0".as_ptr() as *const c_char,
            );
        } else {
            klog_printf(
                slopos_lib::klog::KlogLevel::Debug,
                b"Identity mapping not found (may be normal after early boot)\n\0".as_ptr() as *const c_char,
            );
        }

        klog_printf(
            slopos_lib::klog::KlogLevel::Debug,
            b"Paging system initialized successfully\n\0".as_ptr() as *const c_char,
        );
    }
}

#[no_mangle]
pub extern "C" fn get_memory_layout_info(kernel_virt_base: *mut u64, kernel_phys_base: *mut u64) {
    unsafe {
        if !kernel_virt_base.is_null() {
            *kernel_virt_base = KERNEL_VIRTUAL_BASE;
        }
        if !kernel_phys_base.is_null() {
            *kernel_phys_base = virt_to_phys(KERNEL_VIRTUAL_BASE);
        }
    }
}

#[no_mangle]
pub extern "C" fn is_mapped(vaddr: u64) -> c_int {
    (virt_to_phys(vaddr) != 0) as c_int
}

#[no_mangle]
pub extern "C" fn get_page_size(vaddr: u64) -> u64 {
    unsafe {
        if CURRENT_PAGE_DIR.is_null() || (*CURRENT_PAGE_DIR).pml4.is_null() {
            return 0;
        }
        let pml4_entry = (*(*CURRENT_PAGE_DIR).pml4).entries[pml4_index(vaddr)];
        if !pte_present(pml4_entry) {
            return 0;
        }
        let pdpt = mm_phys_to_virt(pte_address(pml4_entry)) as *mut PageTable;
        let pdpt_entry = (*pdpt).entries[pdpt_index(vaddr)];
        if !pte_present(pdpt_entry) {
            return 0;
        }
        if pte_huge(pdpt_entry) {
            return PAGE_SIZE_1GB;
        }
        let pd = mm_phys_to_virt(pte_address(pdpt_entry)) as *mut PageTable;
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

#[no_mangle]
pub extern "C" fn paging_mark_range_user(
    page_dir: *mut ProcessPageDir,
    start: u64,
    end: u64,
    writable: c_int,
) -> c_int {
    if page_dir.is_null() || unsafe { (*page_dir).pml4.is_null() } || start >= end {
        return -1;
    }
    let mut addr = start & !(PAGE_SIZE_4KB - 1);
    unsafe {
        while addr < end {
            let pml4e = &mut (*(*page_dir).pml4).entries[pml4_index(addr)];
            if !pte_present(*pml4e) {
                return -1;
            }
            if !pte_user(*pml4e) {
                *pml4e |= PAGE_USER;
            }
            let pdpt = mm_phys_to_virt(pte_address(*pml4e)) as *mut PageTable;
            if pdpt.is_null() {
                return -1;
            }
            let pdpte = &mut (*pdpt).entries[pdpt_index(addr)];
            if !pte_present(*pdpte) {
                return -1;
            }
            if pte_huge(*pdpte) {
                let mut flags = *pdpte | PAGE_USER;
                if writable == 0 {
                    flags &= !PAGE_WRITABLE;
                }
                *pdpte = flags;
                addr += PAGE_SIZE_1GB;
                continue;
            }
            let pd = mm_phys_to_virt(pte_address(*pdpte)) as *mut PageTable;
            if pd.is_null() {
                return -1;
            }
            let pde = &mut (*pd).entries[pd_index(addr)];
            if !pte_present(*pde) {
                return -1;
            }
            if pte_huge(*pde) {
                let mut flags = *pde | PAGE_USER;
                if writable == 0 {
                    flags &= !PAGE_WRITABLE;
                }
                *pde = flags;
                addr += PAGE_SIZE_2MB;
                continue;
            }
            let pt = mm_phys_to_virt(pte_address(*pde)) as *mut PageTable;
            if pt.is_null() {
                return -1;
            }
            let pte = &mut (*pt).entries[pt_index(addr)];
            if !pte_present(*pte) {
                return -1;
            }
            let mut flags = *pte | PAGE_USER;
            if writable == 0 {
                flags &= !PAGE_WRITABLE;
            }
            *pte = flags;
            addr += PAGE_SIZE_4KB;
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn paging_is_user_accessible(page_dir: *mut ProcessPageDir, vaddr: u64) -> c_int {
    if page_dir.is_null() || unsafe { (*page_dir).pml4.is_null() } {
        return 0;
    }
    unsafe {
        let pml4_entry = (*(*page_dir).pml4).entries[pml4_index(vaddr)];
        if !pte_present(pml4_entry) || !pte_user(pml4_entry) {
            return 0;
        }
        let pdpt = mm_phys_to_virt(pte_address(pml4_entry)) as *mut PageTable;
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
        let pd = mm_phys_to_virt(pte_address(pdpt_entry)) as *mut PageTable;
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
        let pt = mm_phys_to_virt(pte_address(pd_entry)) as *mut PageTable;
        if pt.is_null() {
            return 0;
        }
        let pt_entry = (*pt).entries[pt_index(vaddr)];
        (pte_present(pt_entry) && pte_user(pt_entry)) as c_int
    }
}

