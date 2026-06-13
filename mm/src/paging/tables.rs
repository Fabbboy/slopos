//! Surviving kernel-side paging surface.
//!
//! Per-process paging functions (`*_in_dir`, COW marker, kernel-mapping
//! sync, `MmTeardownGuard`) have been retired in favour of the OSTD
//! `VmSpace` cursor. The functions in this module are kernel-half only
//! — they write to `KERNEL_PAGE_DIR` (the same physical PML4 OSTD wraps
//! via `KERNEL_VM_SPACE`) and are kept because the priority-10 boot
//! path runs BEFORE `KERNEL_VM_SPACE` is installed at priority 55.
//!
//! Callers that run post-priority-55 should prefer
//! `slopos_mm::kernel_mappings::*` which routes through OSTD's cursor.

use core::cell::SyncUnsafeCell;
use core::ffi::c_int;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use super::page_table_defs::{PAGE_TABLE_ENTRIES, PageTable, PageTableEntry, PageTableLevel};
use crate::paging_defs::PageFlags;
use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::{klog_debug, klog_info};

use super::walker::PageTableWalker;
use crate::hhdm::{self, PhysAddrHhdm};
use crate::memory_layout_defs::KERNEL_VIRTUAL_BASE;
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging_defs::{PAGE_SIZE_2MB, PAGE_SIZE_4KB};

use crate::tlb;

static KERNEL_MAPPING_GEN: AtomicU64 = AtomicU64::new(1);

/// Legacy per-process page-directory descriptor. The per-process
/// paging surface is OSTD-only; this struct survives as a vestigial
/// allocator bookkeeping handle on `ProcessVmInner.page_dir` (its
/// `pml4_phys` is still freed back to the buddy when the process is
/// torn down). The `pml4` pointer points at the legacy kernel-half-only
/// PML4 used by `KERNEL_PAGE_DIR` at boot; per-process PML4s built via
/// `kmalloc(size_of::<ProcessPageDir>())` are zeroed and never
/// installed in CR3 (the OSTD `VmSpace` is the load-bearing half).
#[repr(C)]
pub struct ProcessPageDir {
    /// `KernelSync` wraps the raw pointer so the surrounding struct
    /// auto-derives `Send + Sync`; PML4 ownership is single-process
    /// and access is gated by the per-process `SpinLock` in
    /// `process_vm.rs`.
    pub pml4: slopos_ostd::sync::KernelSync<*mut PageTable>,
    pub pml4_phys: PhysAddr,
    pub ref_count: u32,
    pub process_id: u32,
    /// `KernelSync`-wrapped intrusive next-pointer. The lookup walk in
    /// `kernel_page_dir_walk` runs single-writer pre-SMP for now;
    /// post-SMP migrations use the OSTD `VmSpace` instead.
    pub next: slopos_ostd::sync::KernelSync<*mut ProcessPageDir>,
    pub kernel_mapping_gen: u64,
    pub mm_ctx_id: crate::mmu::MmContextId,
}

impl ProcessPageDir {
    /// Build a fresh per-process page-directory descriptor with default
    /// fields. The caller writes the result into a freshly `kmalloc`'d
    /// slot via [`init_in_kmalloc_slot`](Self::init_in_kmalloc_slot).
    pub fn new(
        pml4: *mut PageTable,
        pml4_phys: PhysAddr,
        process_id: u32,
        mm_ctx_id: crate::mmu::MmContextId,
    ) -> Self {
        Self {
            pml4: slopos_ostd::sync::KernelSync::new(pml4),
            pml4_phys,
            ref_count: 1,
            process_id,
            next: slopos_ostd::sync::KernelSync::new(ptr::null_mut()),
            kernel_mapping_gen: 0,
            mm_ctx_id,
        }
    }

    /// Read `pml4_phys` from a `*mut ProcessPageDir` whose backing slot
    /// was produced by `kmalloc(size_of::<ProcessPageDir>())` +
    /// [`Self::init_in_kmalloc_slot`]. Returns `PhysAddr::NULL` if the
    /// handle is null. The raw-pointer deref is folded into OSTD's
    /// `borrow_ref` helper so this site stays in safe Rust.
    #[inline]
    pub fn pml4_phys_from_raw(page_dir: *mut ProcessPageDir) -> PhysAddr {
        if page_dir.is_null() {
            return PhysAddr::NULL;
        }
        slopos_ostd::util::ptr_buf::borrow_ref::<ProcessPageDir>(page_dir).pml4_phys
    }

    /// Initialise the freshly `kmalloc`'d slot at `dst` with `init`.
    /// Replaces the bare `core::ptr::write(dst, init)` callers used to
    /// write so that the only `unsafe` is the OSTD helper internal to
    /// `slopos_ostd::util::ptr_buf::init_slot`.
    #[inline]
    pub fn init_in_kmalloc_slot(dst: *mut ProcessPageDir, init: ProcessPageDir) {
        slopos_ostd::util::ptr_buf::init_slot(dst, init);
    }

    /// Read the wrapped `pml4: *mut PageTable` for a non-null
    /// `*mut ProcessPageDir`. Callers handle the null check on the
    /// outer page-dir pointer; this routes through OSTD's
    /// `borrow_ref::<ProcessPageDir>` so the field access stays in
    /// safe Rust.
    #[inline]
    fn pml4_ptr_from(page_dir: *mut ProcessPageDir) -> *mut PageTable {
        let dir = slopos_ostd::util::ptr_buf::borrow_ref::<ProcessPageDir>(page_dir);
        *dir.pml4
    }

    /// Borrow the PML4 page-table at `page_dir`'s `pml4` field as a
    /// shared `&PageTable`. Returns `None` if `page_dir` is null or
    /// its `pml4` pointer is null. Folds the chain of raw-pointer
    /// dereferences interior so callers walk in safe Rust.
    #[inline]
    pub fn pml4_table<'a>(page_dir: *mut ProcessPageDir) -> Option<&'a PageTable> {
        if page_dir.is_null() {
            return None;
        }
        let pml4 = Self::pml4_ptr_from(page_dir);
        if pml4.is_null() {
            return None;
        }
        Some(slopos_ostd::util::ptr_buf::borrow_ref::<PageTable>(pml4))
    }

    /// Mutable variant of [`Self::pml4_table`]. Caller is responsible
    /// for ensuring single-writer access to the PML4 (typically by
    /// holding the per-process slot lock in `process_vm.rs`).
    #[inline]
    pub fn pml4_table_mut<'a>(page_dir: *mut ProcessPageDir) -> Option<&'a mut PageTable> {
        if page_dir.is_null() {
            return None;
        }
        let pml4 = Self::pml4_ptr_from(page_dir);
        if pml4.is_null() {
            return None;
        }
        Some(slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(
            pml4,
        ))
    }
}

static KERNEL_PAGE_DIR: SyncUnsafeCell<ProcessPageDir> = SyncUnsafeCell::new(ProcessPageDir {
    pml4: slopos_ostd::sync::KernelSync::new(ptr::null_mut()),
    pml4_phys: PhysAddr::NULL,
    ref_count: 1,
    process_id: 0,
    next: slopos_ostd::sync::KernelSync::new(ptr::null_mut()),
    kernel_mapping_gen: 0,
    mm_ctx_id: crate::mmu::MmContextId::INVALID,
});

fn table_empty(table: &PageTable) -> bool {
    table.iter().all(|e| !e.is_present())
}

fn alloc_page_table() -> Option<(PhysAddr, *mut PageTable)> {
    let phys = alloc_kernel_page();
    if phys.is_null() {
        return None;
    }
    let virt = phys.to_virt().as_mut_ptr::<PageTable>();
    if virt.is_null() {
        free_page_frame(phys);
        return None;
    }
    // Freshly allocated, exclusively owned page-table page; the `&mut`
    // reborrow lives in OSTD's `borrow_ref_mut` helper.
    slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(virt).zero();
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
fn leaf_flags_with_global(flags: PageFlags, user_mapping: bool) -> PageFlags {
    if user_mapping {
        flags
    } else {
        flags | PageFlags::GLOBAL
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

    // Freshly allocated `pd_ptr` is exclusively owned; the `&mut`
    // reborrow lives in OSTD's `borrow_ref_mut`.
    let pd_table = slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(pd_ptr);
    for i in 0..PAGE_TABLE_ENTRIES {
        let phys = huge_phys.offset(i as u64 * PAGE_SIZE_2MB);
        let entry = pd_table.entry_mut(i);
        entry.set(phys, huge_flags | PageFlags::HUGE);
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

    let pt_table = slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(pt_ptr);
    for i in 0..PAGE_TABLE_ENTRIES {
        let phys = huge_phys.offset(i as u64 * PAGE_SIZE_4KB);
        let entry = pt_table.entry_mut(i);
        entry.set(phys, huge_flags);
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
fn flush_kernel_page_after_mod(vaddr: VirtAddr) {
    tlb::flush_page(vaddr);
}

#[inline(always)]
fn get_cr3() -> PhysAddr {
    crate::mmu::read_cr3_value().pml4_phys()
}

pub fn paging_bump_kernel_mapping_gen() {
    KERNEL_MAPPING_GEN.fetch_add(1, Ordering::Release);
}

/// Walk the kernel half (PML4 indices 256..512) via the OSTD cursor
/// and stamp `GLOBAL` onto every present leaf — handles 4 KiB / 2 MiB /
/// 1 GiB leaves uniformly via the cursor's `protect::<S>` API.
/// Idempotent. Used by the early boot init step that wraps the live
/// kernel-master PML4.
pub fn paging_mark_kernel_global() {
    crate::kernel_mappings::mark_kernel_global();
}

fn virt_to_phys_for_dir(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> PhysAddr {
    let Some(pml4) = ProcessPageDir::pml4_table(page_dir) else {
        return PhysAddr::NULL;
    };
    let walker = PageTableWalker::new();
    match walker.walk(pml4, vaddr) {
        Ok(result) => result.phys_addr,
        Err(_) => PhysAddr::NULL,
    }
}

pub fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    virt_to_phys_for_dir(KERNEL_PAGE_DIR.get(), vaddr)
}

fn map_page_in_directory(
    page_dir: *mut ProcessPageDir,
    vaddr: VirtAddr,
    paddr: PhysAddr,
    flags: u64,
    page_size: u64,
) -> c_int {
    if !vaddr.is_aligned(page_size) || !paddr.is_aligned(page_size) {
        return -1;
    }

    let flags = PageFlags::from_bits_truncate(flags);
    let user_mapping = flags.contains(PageFlags::USER) && is_user_address(vaddr);
    let inter_flags = intermediate_flags(user_mapping);

    // Single-writer access to the kernel-half PML4 tree; the `&mut`
    // reborrows live in OSTD's `borrow_ref_mut`.
    let Some(pml4_table) = ProcessPageDir::pml4_table_mut(page_dir) else {
        return -1;
    };

    let pml4_idx = PageTableLevel::Four.index_of(vaddr);
    let pdpt_idx = PageTableLevel::Three.index_of(vaddr);
    let pd_idx = PageTableLevel::Two.index_of(vaddr);
    let pt_idx = PageTableLevel::One.index_of(vaddr);

    let pml4_entry = pml4_table.entry_mut(pml4_idx);
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

    let pdpt_table = slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(pdpt);
    let pdpt_entry = pdpt_table.entry_mut(pdpt_idx);

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
    } else if pdpt_entry.is_huge() {
        let Some(ptr) = split_pdpt_huge(pdpt_entry) else {
            return -1;
        };
        ptr
    } else {
        if user_mapping && !pdpt_entry.is_user() {
            pdpt_entry.add_flags(PageFlags::USER);
        }
        phys_to_table(pdpt_entry.address())
    };

    let pd_table = slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(pd);
    let pd_entry = pd_table.entry_mut(pd_idx);

    if page_size == PAGE_SIZE_2MB {
        if pd_entry.is_present() {
            return -1;
        }
        pd_entry.set(
            paddr,
            leaf_flags_with_global(flags, user_mapping) | PageFlags::PRESENT | PageFlags::HUGE,
        );
        flush_kernel_page_after_mod(vaddr);
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
    } else if pd_entry.is_huge() {
        let Some(ptr) = split_pd_huge(pd_entry) else {
            return -1;
        };
        ptr
    } else {
        if user_mapping && !pd_entry.is_user() {
            pd_entry.add_flags(PageFlags::USER);
        }
        phys_to_table(pd_entry.address())
    };

    let pt_table = slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(pt);
    let pt_entry = pt_table.entry_mut(pt_idx);

    let was_present = pt_entry.is_present();
    if was_present {
        let old_phys = pt_entry.address();
        if !old_phys.is_null() {
            free_page_frame(old_phys);
        }
    }

    pt_entry.set(
        paddr,
        leaf_flags_with_global(flags, user_mapping) | PageFlags::PRESENT,
    );

    if was_present {
        flush_kernel_page_after_mod(vaddr);
    }
    0
}

pub fn map_page_4kb(vaddr: VirtAddr, paddr: PhysAddr, flags: u64) -> c_int {
    map_page_in_directory(KERNEL_PAGE_DIR.get(), vaddr, paddr, flags, PAGE_SIZE_4KB)
}

fn unmap_page_in_directory(page_dir: *mut ProcessPageDir, vaddr: VirtAddr) -> PhysAddr {
    let Some(pml4_table) = ProcessPageDir::pml4_table_mut(page_dir) else {
        return PhysAddr::NULL;
    };

    let pml4_idx = PageTableLevel::Four.index_of(vaddr);
    let pdpt_idx = PageTableLevel::Three.index_of(vaddr);
    let pd_idx = PageTableLevel::Two.index_of(vaddr);
    let pt_idx = PageTableLevel::One.index_of(vaddr);

    let pml4_entry = pml4_table.entry_mut(pml4_idx);
    if !pml4_entry.is_present() {
        return PhysAddr::NULL;
    }
    let pml4_entry_phys = pml4_entry.address();

    let pdpt_ptr = phys_to_table(pml4_entry_phys);
    let pdpt_table = slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(pdpt_ptr);
    let pdpt_entry = pdpt_table.entry_mut(pdpt_idx);
    if !pdpt_entry.is_present() {
        return PhysAddr::NULL;
    }

    if pdpt_entry.is_huge() {
        let phys = pdpt_entry.address();
        pdpt_entry.clear();
        flush_kernel_page_after_mod(vaddr);
        // Re-borrow `pdpt_table` after dropping `pdpt_entry`.
        let pdpt_empty = {
            let t = slopos_ostd::util::ptr_buf::borrow_ref::<PageTable>(pdpt_ptr);
            table_empty(t)
        };
        if pdpt_empty {
            pml4_entry.clear();
            free_page_frame(pml4_entry_phys);
        }
        return phys;
    }

    let pdpt_entry_phys = pdpt_entry.address();
    let pd_ptr = phys_to_table(pdpt_entry_phys);
    let pd_table = slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(pd_ptr);
    let pd_entry = pd_table.entry_mut(pd_idx);
    if !pd_entry.is_present() {
        return PhysAddr::NULL;
    }

    let unmapped_phys;

    if pd_entry.is_huge() {
        unmapped_phys = pd_entry.address();
        pd_entry.clear();
        flush_kernel_page_after_mod(vaddr);
    } else {
        let pd_entry_phys = pd_entry.address();
        let pt_ptr = phys_to_table(pd_entry_phys);
        if pt_ptr.is_null() {
            return PhysAddr::NULL;
        }
        let pt_table = slopos_ostd::util::ptr_buf::borrow_ref_mut::<PageTable>(pt_ptr);
        let pt_entry = pt_table.entry_mut(pt_idx);
        if pt_entry.is_present() {
            unmapped_phys = pt_entry.address();
            pt_entry.clear();
            flush_kernel_page_after_mod(vaddr);
        } else {
            unmapped_phys = PhysAddr::NULL;
        }
        let pt_empty = {
            let t = slopos_ostd::util::ptr_buf::borrow_ref::<PageTable>(pt_ptr);
            table_empty(t)
        };
        if pt_empty {
            pd_entry.clear();
            free_page_frame(pd_entry_phys);
        }
    }

    let pd_empty = {
        let t = slopos_ostd::util::ptr_buf::borrow_ref::<PageTable>(pd_ptr);
        table_empty(t)
    };
    if pd_empty {
        pdpt_entry.clear();
        free_page_frame(pdpt_entry_phys);
    }

    let pdpt_empty = {
        let t = slopos_ostd::util::ptr_buf::borrow_ref::<PageTable>(pdpt_ptr);
        table_empty(t)
    };
    if pdpt_empty {
        pml4_entry.clear();
        free_page_frame(pml4_entry_phys);
    }

    unmapped_phys
}

pub fn unmap_page(vaddr: VirtAddr) -> PhysAddr {
    unmap_page_in_directory(KERNEL_PAGE_DIR.get(), vaddr)
}

pub fn paging_get_kernel_directory() -> *mut ProcessPageDir {
    KERNEL_PAGE_DIR.get()
}

pub fn init_paging() {
    let cr3 = get_cr3();
    let kernel_dir =
        slopos_ostd::util::ptr_buf::borrow_ref_mut::<ProcessPageDir>(KERNEL_PAGE_DIR.get());
    kernel_dir.pml4_phys = cr3;

    let pml4_ptr = phys_to_table(kernel_dir.pml4_phys);
    if pml4_ptr.is_null() {
        panic!("Failed to translate kernel PML4 physical address");
    }
    kernel_dir.pml4 = slopos_ostd::sync::KernelSync::new(pml4_ptr);

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

pub fn get_memory_layout_info(kernel_virt_base: *mut u64, kernel_phys_base: *mut u64) {
    slopos_ostd::util::ptr_buf::nullable_write(kernel_virt_base, KERNEL_VIRTUAL_BASE);
    slopos_ostd::util::ptr_buf::nullable_write(
        kernel_phys_base,
        virt_to_phys(VirtAddr::new(KERNEL_VIRTUAL_BASE)).as_u64(),
    );
}

pub fn is_mapped(vaddr: VirtAddr) -> c_int {
    (!virt_to_phys(vaddr).is_null()) as c_int
}

pub fn get_page_size(vaddr: VirtAddr) -> u64 {
    let Some(pml4) = ProcessPageDir::pml4_table(KERNEL_PAGE_DIR.get()) else {
        return 0;
    };
    let walker = PageTableWalker::new();
    match walker.walk(pml4, vaddr) {
        Ok(result) => result.page_size,
        Err(_) => 0,
    }
}
