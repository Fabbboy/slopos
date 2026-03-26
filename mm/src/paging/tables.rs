use core::cell::SyncUnsafeCell;
use core::ffi::c_int;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use super::page_table_defs::{PAGE_TABLE_ENTRIES, PageTable, PageTableEntry, PageTableLevel};
use crate::paging_defs::PageFlags;
use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_arch::cpu;
use slopos_utils::{klog_debug, klog_info};

use super::walker::{PageTableWalker, WalkAction};
use crate::hhdm::{self, PhysAddrHhdm};
use crate::memory_layout_defs::KERNEL_VIRTUAL_BASE;
use crate::page_alloc::{
    ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame, page_frame_can_free, page_frame_is_tracked,
};
use crate::paging_defs::{PAGE_SIZE_1GB, PAGE_SIZE_2MB, PAGE_SIZE_4KB};

use crate::tlb;

static KERNEL_MAPPING_GEN: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
pub struct ProcessPageDir {
    pub pml4: *mut PageTable,
    pub pml4_phys: PhysAddr,
    pub ref_count: u32,
    pub process_id: u32,
    pub next: *mut ProcessPageDir,
    pub kernel_mapping_gen: u64,
}

unsafe impl Send for ProcessPageDir {}
unsafe impl Sync for ProcessPageDir {}

pub static EARLY_PML4: SyncUnsafeCell<PageTable> = SyncUnsafeCell::new(PageTable::EMPTY);
pub static EARLY_PDPT: SyncUnsafeCell<PageTable> = SyncUnsafeCell::new(PageTable::EMPTY);
pub static EARLY_PD: SyncUnsafeCell<PageTable> = SyncUnsafeCell::new(PageTable::EMPTY);

static KERNEL_PAGE_DIR: SyncUnsafeCell<ProcessPageDir> = SyncUnsafeCell::new(ProcessPageDir {
    pml4: ptr::null_mut(),
    pml4_phys: PhysAddr::NULL,
    ref_count: 1,
    process_id: 0,
    next: ptr::null_mut(),
    kernel_mapping_gen: 0,
});

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

fn table_flags_from_leaf(leaf_flags: PageFlags) -> PageFlags {
    let mut flags = PageFlags::PRESENT;
    if leaf_flags.contains(PageFlags::WRITABLE) {
        flags |= PageFlags::WRITABLE;
    }
    if leaf_flags.contains(PageFlags::USER) {
        flags |= PageFlags::USER;
    }
    flags
}

fn split_pdpt_huge(pdpt_entry: &mut PageTableEntry) -> Option<*mut PageTable> {
    if !pdpt_entry.is_present() || !pdpt_entry.is_huge() {
        return Some(phys_to_table(pdpt_entry.address()));
    }

    let huge_phys = pdpt_entry.address();
    let huge_flags = pdpt_entry.flags();
    let Some((pd_phys, pd_ptr)) = alloc_page_table() else {
        return None;
    };

    unsafe {
        for i in 0..PAGE_TABLE_ENTRIES {
            let phys = huge_phys.offset(i as u64 * PAGE_SIZE_2MB);
            let entry = (*pd_ptr).entry_mut(i);
            entry.set(phys, huge_flags | PageFlags::HUGE);
        }
    }

    pdpt_entry.set(pd_phys, table_flags_from_leaf(huge_flags));
    Some(pd_ptr)
}

fn split_pd_huge(pd_entry: &mut PageTableEntry) -> Option<*mut PageTable> {
    if !pd_entry.is_present() || !pd_entry.is_huge() {
        return Some(phys_to_table(pd_entry.address()));
    }

    let huge_phys = pd_entry.address();
    let mut huge_flags = pd_entry.flags();
    huge_flags.remove(PageFlags::HUGE);
    let Some((pt_phys, pt_ptr)) = alloc_page_table() else {
        return None;
    };

    unsafe {
        for i in 0..PAGE_TABLE_ENTRIES {
            let phys = huge_phys.offset(i as u64 * PAGE_SIZE_4KB);
            let entry = (*pt_ptr).entry_mut(i);
            entry.set(phys, huge_flags);
        }
    }

    pd_entry.set(pt_phys, table_flags_from_leaf(huge_flags));
    Some(pt_ptr)
}

#[inline]
fn phys_to_table(phys: PhysAddr) -> *mut PageTable {
    phys.to_virt().as_mut_ptr()
}

fn is_user_address(vaddr: VirtAddr) -> bool {
    let raw = vaddr.as_u64();
    raw < KERNEL_VIRTUAL_BASE && raw >= crate::memory_layout_defs::USER_SPACE_START_VA
}

#[inline]
fn flush_page_for_directory(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) {
    if !page_dir.is_null() && is_user_address(vaddr) {
        // User-address modifications in a process page directory only
        // need a local INVLPG.  Other CPUs running this process will
        // pick up the change when they context-switch in (CR3 reload
        // flushes the entire TLB) or via exit_lazy_tlb's generation
        // check.  This avoids the broadcast IPI which (a) is wasteful
        // since all processes share the same user virtual ranges and
        // (b) cannot safely wait for acks while the caller holds a
        // page-table lock.
        //
        // Bump the process TLB generation so remote CPUs know their
        // cached entries are stale.
        let pid = unsafe { (*page_dir).process_id };
        tlb::flush_page_local_for_process(pid, vaddr);
    } else {
        // Kernel-space modifications are shared across all page tables,
        // so a broadcast is necessary.
        tlb::flush_page(vaddr);
    }
}

#[inline(always)]
fn get_cr3() -> PhysAddr {
    PhysAddr::new(cpu::read_cr3() & !0xFFF)
}

#[inline(always)]
fn set_cr3(pml4_phys: PhysAddr) {
    cpu::write_cr3(pml4_phys.as_u64());
}

pub fn paging_copy_kernel_mappings(dest_pml4: *mut PageTable) {
    if dest_pml4.is_null() {
        return;
    }
    unsafe {
        if (*KERNEL_PAGE_DIR.get()).pml4.is_null() {
            klog_info!("paging_copy_kernel_mappings: Kernel PML4 unavailable");
            return;
        }
        for i in 0..512 {
            *(&mut *dest_pml4).entry_mut(i) = *(&*(*KERNEL_PAGE_DIR.get()).pml4).entry(i);
        }
        for i in 0..256 {
            *(&mut *dest_pml4).entry_mut(i) = PageTableEntry::EMPTY;
        }
    }
}

pub fn paging_sync_kernel_mappings(page_dir: *mut ProcessPageDir) {
    if page_dir.is_null() {
        return;
    }
    let current_gen = KERNEL_MAPPING_GEN.load(Ordering::Acquire);
    unsafe {
        if (*page_dir).kernel_mapping_gen == current_gen {
            return;
        }
        let dest_pml4 = (*page_dir).pml4;
        if dest_pml4.is_null() || (*KERNEL_PAGE_DIR.get()).pml4.is_null() {
            return;
        }
        for i in 256..512 {
            *(&mut *dest_pml4).entry_mut(i) = *(&*(*KERNEL_PAGE_DIR.get()).pml4).entry(i);
        }
        (*page_dir).kernel_mapping_gen = current_gen;
    }
}

pub fn paging_bump_kernel_mapping_gen() {
    KERNEL_MAPPING_GEN.fetch_add(1, Ordering::Release);
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
    virt_to_phys_for_dir(KERNEL_PAGE_DIR.get(), vaddr)
}

pub fn virt_to_phys_process(vaddr: VirtAddr, page_dir: *mut ProcessPageDir) -> PhysAddr {
    virt_to_phys_for_dir(page_dir, vaddr)
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
                klog_info!(
                    "Paging: Failed to allocate PDPT for vaddr 0x{:x}",
                    vaddr.as_u64()
                );
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
            flush_page_for_directory(page_dir, vaddr);
            return 0;
        }

        let pd = if !pdpt_entry.is_present() {
            let Some((phys, ptr)) = alloc_page_table() else {
                klog_info!(
                    "Paging: Failed to allocate PD for vaddr 0x{:x}",
                    vaddr.as_u64()
                );
                return -1;
            };
            pdpt_entry.set(phys, inter_flags);
            ptr
        } else {
            if pdpt_entry.is_huge() {
                let Some(ptr) = split_pdpt_huge(pdpt_entry) else {
                    return -1;
                };
                ptr
            } else {
                if user_mapping && !pdpt_entry.is_user() {
                    pdpt_entry.add_flags(PageFlags::USER);
                }
                phys_to_table(pdpt_entry.address())
            }
        };

        let pd_entry = (&mut *pd).entry_mut(pd_idx);

        if page_size == PAGE_SIZE_2MB {
            if pd_entry.is_present() {
                return -1;
            }
            pd_entry.set(paddr, flags | PageFlags::PRESENT | PageFlags::HUGE);
            flush_page_for_directory(page_dir, vaddr);
            return 0;
        }

        let pt = if !pd_entry.is_present() {
            let Some((phys, ptr)) = alloc_page_table() else {
                klog_info!(
                    "Paging: Failed to allocate PT for vaddr 0x{:x}",
                    vaddr.as_u64()
                );
                return -1;
            };
            pd_entry.set(phys, inter_flags);
            ptr
        } else {
            if pd_entry.is_huge() {
                let Some(ptr) = split_pd_huge(pd_entry) else {
                    return -1;
                };
                ptr
            } else {
                if user_mapping && !pd_entry.is_user() {
                    pd_entry.add_flags(PageFlags::USER);
                }
                phys_to_table(pd_entry.address())
            }
        };

        let pt_entry = (&mut *pt).entry_mut(pt_idx);

        let was_present = pt_entry.is_present();
        if was_present {
            let old_phys = pt_entry.address();
            if !old_phys.is_null() && page_frame_can_free(old_phys) != 0 {
                free_page_frame(old_phys);
            }
        }

        pt_entry.set(paddr, flags | PageFlags::PRESENT);

        if was_present {
            flush_page_for_directory(page_dir, vaddr);
        }
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
    map_page_in_directory(KERNEL_PAGE_DIR.get(), vaddr, paddr, flags, PAGE_SIZE_4KB)
}

pub fn map_page_2mb(vaddr: VirtAddr, paddr: PhysAddr, flags: u64) -> c_int {
    map_page_in_directory(KERNEL_PAGE_DIR.get(), vaddr, paddr, flags, PAGE_SIZE_2MB)
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

    let phys = virt_to_phys_in_dir(KERNEL_PAGE_DIR.get(), kernel_vaddr);
    if phys.is_null() {
        return -1;
    }
    map_page_4kb_in_dir(page_dir, user_vaddr, phys, flags | PageFlags::USER.bits())
}

fn unmap_page_in_directory(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> PhysAddr {
    if page_dir.is_null() {
        return PhysAddr::NULL;
    }
    unsafe {
        let pml4 = (*page_dir).pml4;
        if pml4.is_null() {
            return PhysAddr::NULL;
        }

        let pml4_idx = PageTableLevel::Four.index_of(vaddr);
        let pdpt_idx = PageTableLevel::Three.index_of(vaddr);
        let pd_idx = PageTableLevel::Two.index_of(vaddr);
        let pt_idx = PageTableLevel::One.index_of(vaddr);

        let pml4_entry = (&mut *pml4).entry_mut(pml4_idx);
        if !pml4_entry.is_present() {
            return PhysAddr::NULL;
        }
        let pml4_entry_phys = pml4_entry.address();

        let pdpt = phys_to_table(pml4_entry_phys);
        let pdpt_entry = (&mut *pdpt).entry_mut(pdpt_idx);
        if !pdpt_entry.is_present() {
            return PhysAddr::NULL;
        }

        if pdpt_entry.is_huge() {
            let phys = pdpt_entry.address();
            pdpt_entry.clear();
            flush_page_for_directory(page_dir, vaddr);
            if table_empty(&*pdpt) {
                pml4_entry.clear();
                if page_frame_can_free(pml4_entry_phys) != 0 {
                    free_page_frame(pml4_entry_phys);
                }
            }
            return phys;
        }

        let pdpt_entry_phys = pdpt_entry.address();
        let pd = phys_to_table(pdpt_entry_phys);
        let pd_entry = (&mut *pd).entry_mut(pd_idx);
        if !pd_entry.is_present() {
            return PhysAddr::NULL;
        }

        let unmapped_phys;

        if pd_entry.is_huge() {
            unmapped_phys = pd_entry.address();
            pd_entry.clear();
            flush_page_for_directory(page_dir, vaddr);
        } else {
            let pd_entry_phys = pd_entry.address();
            let pt = phys_to_table(pd_entry_phys);
            if pt.is_null() {
                return PhysAddr::NULL;
            }
            let pt_entry = (&mut *pt).entry_mut(pt_idx);
            if pt_entry.is_present() {
                unmapped_phys = pt_entry.address();
                pt_entry.clear();
                flush_page_for_directory(page_dir, vaddr);
            } else {
                unmapped_phys = PhysAddr::NULL;
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

        unmapped_phys
    }
}

pub fn unmap_page_in_dir(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> PhysAddr {
    unmap_page_in_directory(page_dir, vaddr)
}

pub fn unmap_page(vaddr: VirtAddr) -> PhysAddr {
    unmap_page_in_directory(KERNEL_PAGE_DIR.get(), vaddr)
}

pub fn switch_page_directory(page_dir: *mut ProcessPageDir) -> c_int {
    if page_dir.is_null() {
        return -1;
    }
    let cpu = slopos_arch::pcr::get_current_cpu();
    let old_process_id = tlb::current_process_on_cpu(cpu);
    unsafe {
        set_cr3((*page_dir).pml4_phys);
        let new_process_id = if (*page_dir).process_id == 0 {
            INVALID_PROCESS_ID
        } else {
            (*page_dir).process_id
        };
        tlb::notify_mm_switch(old_process_id, new_process_id, cpu);
    }
    0
}

pub fn paging_get_kernel_directory() -> *mut ProcessPageDir {
    KERNEL_PAGE_DIR.get()
}

/// Recursively free a page table hierarchy from the given level down.
///
/// - At the leaf level (PT/Level 1): frees each mapped physical frame.
/// - At intermediate levels (PD/Level 2, PDPT/Level 3): recurses into
///   non-huge subtables and frees huge-page frames directly.
/// - Always frees the table's own frame at the end.
unsafe fn free_table_level(table: *mut PageTable, table_phys: PhysAddr, level: PageTableLevel) {
    if table.is_null() {
        return;
    }

    for entry in (*table).iter() {
        if !entry.is_present() {
            continue;
        }

        let phys = entry.address();
        if level == PageTableLevel::One {
            if page_frame_can_free(phys) != 0 {
                free_page_frame(phys);
            }
            continue;
        }

        if entry.is_huge() {
            if page_frame_is_tracked(phys) != 0 {
                free_page_frame(phys);
            }
            continue;
        }

        let next_table = entry.table_ptr();
        let next_level = level.next_lower().unwrap();
        free_table_level(next_table, phys, next_level);
    }

    if page_frame_can_free(table_phys) != 0 {
        free_page_frame(table_phys);
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
        // Only free user space entries (0-255). Higher-half entries (256-511)
        // are shared kernel mappings copied from KERNEL_PAGE_DIR.
        for pml4_idx in 0..256 {
            let entry = (&mut *pml4).entry_mut(pml4_idx);
            if !entry.is_present() {
                continue;
            }
            let pdpt_phys = entry.address();
            let pdpt = phys_to_table(pdpt_phys);
            free_table_level(pdpt, pdpt_phys, PageTableLevel::Three);
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
        (*KERNEL_PAGE_DIR.get()).pml4_phys = cr3;

        let pml4_ptr = phys_to_table((*KERNEL_PAGE_DIR.get()).pml4_phys);
        if pml4_ptr.is_null() {
            panic!("Failed to translate kernel PML4 physical address");
        }
        (*KERNEL_PAGE_DIR.get()).pml4 = pml4_ptr;

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
        let page_dir = KERNEL_PAGE_DIR.get();
        if (*page_dir).pml4.is_null() {
            return 0;
        }
        let pml4 = (*page_dir).pml4;
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

            let pdpt = pml4_entry.table_ptr();
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

            let pd = pdpt_entry.table_ptr();
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

            let pt = pd_entry.table_ptr();
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

/// Update page table protection flags for an existing mapped range.
///
/// Sets or clears WRITABLE and NO_EXECUTE on each present 4KB PTE in
/// `[start, end)`. Skips pages that are not yet mapped (lazy/demand).
/// Returns 0 on success, -1 if page_dir is invalid.
pub fn paging_update_range_protection(
    page_dir: *mut ProcessPageDir,
    start: VirtAddr,
    end: VirtAddr,
    new_flags: PageFlags,
) -> c_int {
    if page_dir.is_null() || unsafe { (*page_dir).pml4.is_null() } || start.as_u64() >= end.as_u64()
    {
        return -1;
    }

    let writable = new_flags.contains(PageFlags::WRITABLE);
    let no_execute = new_flags.contains(PageFlags::NO_EXECUTE);

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
                addr += PAGE_SIZE_4KB;
                continue;
            }

            let pdpt = pml4_entry.table_ptr();
            if pdpt.is_null() {
                addr += PAGE_SIZE_4KB;
                continue;
            }
            let pdpt_entry = (&mut *pdpt).entry_mut(pdpt_idx);
            if !pdpt_entry.is_present() {
                addr += PAGE_SIZE_4KB;
                continue;
            }

            let pd = pdpt_entry.table_ptr();
            if pd.is_null() {
                addr += PAGE_SIZE_4KB;
                continue;
            }
            let pd_entry = (&mut *pd).entry_mut(pd_idx);
            if !pd_entry.is_present() {
                addr += PAGE_SIZE_4KB;
                continue;
            }

            let pt = pd_entry.table_ptr();
            if pt.is_null() {
                addr += PAGE_SIZE_4KB;
                continue;
            }
            let pt_entry = (&mut *pt).entry_mut(pt_idx);
            if pt_entry.is_present() {
                if writable {
                    pt_entry.add_flags(PageFlags::WRITABLE);
                } else {
                    pt_entry.remove_flags(PageFlags::WRITABLE);
                }
                if no_execute {
                    pt_entry.add_flags(PageFlags::NO_EXECUTE);
                } else {
                    pt_entry.remove_flags(PageFlags::NO_EXECUTE);
                }
                flush_page_for_directory(page_dir, vaddr);
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

pub fn paging_mark_cow(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> c_int {
    if page_dir.is_null() || unsafe { (*page_dir).pml4.is_null() } {
        return -1;
    }

    let aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !(PAGE_SIZE_4KB - 1));

    unsafe {
        let pml4 = (*page_dir).pml4;
        let pml4_idx = PageTableLevel::Four.index_of(aligned_vaddr);
        let pdpt_idx = PageTableLevel::Three.index_of(aligned_vaddr);
        let pd_idx = PageTableLevel::Two.index_of(aligned_vaddr);
        let pt_idx = PageTableLevel::One.index_of(aligned_vaddr);

        let pml4_entry = (&*pml4).entry(pml4_idx);
        if !pml4_entry.is_present() {
            return -1;
        }

        let pdpt = pml4_entry.table_ptr();
        let pdpt_entry = (&*pdpt).entry(pdpt_idx);
        if !pdpt_entry.is_present() {
            return -1;
        }
        if pdpt_entry.is_huge() {
            return -1;
        }

        let pd = pdpt_entry.table_ptr();
        let pd_entry = (&*pd).entry(pd_idx);
        if !pd_entry.is_present() {
            return -1;
        }
        if pd_entry.is_huge() {
            return -1;
        }

        let pt = pd_entry.table_ptr();
        let pt_entry = (&mut *pt).entry_mut(pt_idx);
        if !pt_entry.is_present() {
            return -1;
        }

        pt_entry.remove_flags(PageFlags::WRITABLE);
        pt_entry.add_flags(PageFlags::COW);
    }

    0
}

/// Resolve a COW page by making it writable in-place (no page copy, no free).
/// Used when the page's refcount is 1 — just flip the flags on the existing PTE.
pub fn paging_resolve_cow(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> c_int {
    if page_dir.is_null() || unsafe { (*page_dir).pml4.is_null() } {
        return -1;
    }

    let aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !(PAGE_SIZE_4KB - 1));

    unsafe {
        let pml4 = (*page_dir).pml4;
        let pml4_idx = PageTableLevel::Four.index_of(aligned_vaddr);
        let pdpt_idx = PageTableLevel::Three.index_of(aligned_vaddr);
        let pd_idx = PageTableLevel::Two.index_of(aligned_vaddr);
        let pt_idx = PageTableLevel::One.index_of(aligned_vaddr);

        let pml4_entry = (&*pml4).entry(pml4_idx);
        if !pml4_entry.is_present() {
            return -1;
        }

        let pdpt = pml4_entry.table_ptr();
        let pdpt_entry = (&*pdpt).entry(pdpt_idx);
        if !pdpt_entry.is_present() {
            return -1;
        }
        if pdpt_entry.is_huge() {
            return -1;
        }

        let pd = pdpt_entry.table_ptr();
        let pd_entry = (&*pd).entry(pd_idx);
        if !pd_entry.is_present() {
            return -1;
        }
        if pd_entry.is_huge() {
            return -1;
        }

        let pt = pd_entry.table_ptr();
        let pt_entry = (&mut *pt).entry_mut(pt_idx);
        if !pt_entry.is_present() {
            return -1;
        }

        pt_entry.remove_flags(PageFlags::COW);
        pt_entry.add_flags(PageFlags::WRITABLE);
        flush_page_for_directory(page_dir, aligned_vaddr);
    }

    0
}

pub fn paging_is_cow(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> bool {
    if page_dir.is_null() || unsafe { (*page_dir).pml4.is_null() } {
        return false;
    }

    let aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !(PAGE_SIZE_4KB - 1));

    unsafe {
        let pml4 = (*page_dir).pml4;
        let pml4_idx = PageTableLevel::Four.index_of(aligned_vaddr);
        let pdpt_idx = PageTableLevel::Three.index_of(aligned_vaddr);
        let pd_idx = PageTableLevel::Two.index_of(aligned_vaddr);
        let pt_idx = PageTableLevel::One.index_of(aligned_vaddr);

        let pml4_entry = (&*pml4).entry(pml4_idx);
        if !pml4_entry.is_present() {
            return false;
        }

        let pdpt = pml4_entry.table_ptr();
        let pdpt_entry = (&*pdpt).entry(pdpt_idx);
        if !pdpt_entry.is_present() {
            return false;
        }
        if pdpt_entry.is_huge() {
            return false;
        }

        let pd = pdpt_entry.table_ptr();
        let pd_entry = (&*pd).entry(pd_idx);
        if !pd_entry.is_present() {
            return false;
        }
        if pd_entry.is_huge() {
            return false;
        }

        let pt = pd_entry.table_ptr();
        let pt_entry = (&*pt).entry(pt_idx);
        if !pt_entry.is_present() {
            return false;
        }

        pt_entry.flags().contains(PageFlags::COW)
    }
}

pub fn paging_get_pte_flags(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> Option<PageFlags> {
    if page_dir.is_null() || unsafe { (*page_dir).pml4.is_null() } {
        return None;
    }

    let aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !(PAGE_SIZE_4KB - 1));

    unsafe {
        let pml4 = (*page_dir).pml4;
        let pml4_idx = PageTableLevel::Four.index_of(aligned_vaddr);
        let pdpt_idx = PageTableLevel::Three.index_of(aligned_vaddr);
        let pd_idx = PageTableLevel::Two.index_of(aligned_vaddr);
        let pt_idx = PageTableLevel::One.index_of(aligned_vaddr);

        let pml4_entry = (&*pml4).entry(pml4_idx);
        if !pml4_entry.is_present() {
            return None;
        }

        let pdpt = pml4_entry.table_ptr();
        let pdpt_entry = (&*pdpt).entry(pdpt_idx);
        if !pdpt_entry.is_present() {
            return None;
        }
        if pdpt_entry.is_huge() {
            return Some(pdpt_entry.flags());
        }

        let pd = pdpt_entry.table_ptr();
        let pd_entry = (&*pd).entry(pd_idx);
        if !pd_entry.is_present() {
            return None;
        }
        if pd_entry.is_huge() {
            return Some(pd_entry.flags());
        }

        let pt = pd_entry.table_ptr();
        let pt_entry = (&*pt).entry(pt_idx);
        if !pt_entry.is_present() {
            return None;
        }

        Some(pt_entry.flags())
    }
}
