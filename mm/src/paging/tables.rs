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

use super::page_table_defs::{
    PAGE_TABLE_ENTRIES, PageTableEntry, PageTableLevel, entry_at, set_entry_at, table_empty_at,
    unlink_child, zero_table_at,
};
use crate::paging_defs::PageFlags;
use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::{klog_debug, klog_info};

use super::walker::walk_phys;
use crate::hhdm::{self, PhysAddrHhdm};
use crate::memory_layout_defs::KERNEL_VIRTUAL_BASE;
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging_defs::{PAGE_SIZE_2MB, PAGE_SIZE_4KB};

use crate::tlb;

static KERNEL_MAPPING_GEN: AtomicU64 = AtomicU64::new(1);

/// PML4, PDPT, PD, PT — the depth of the descent, and the width of the
/// path array the prune walks back up.
const PAGE_TABLE_LEVELS: usize = 4;

/// Per-process page-directory descriptor: a PML4 frame's physical
/// address plus the bookkeeping that tracks it.
///
/// The load-bearing per-process paging surface is the OSTD `VmSpace`;
/// what survives here is frame accounting. A per-process descriptor
/// lives in a `kmalloc(size_of::<ProcessPageDir>())` slot reached
/// through `ProcessVm.page_dir`, and its `pml4_phys` frame is allocated
/// with the process, zeroed, never installed in CR3, and freed back to
/// the buddy at teardown. `KERNEL_PAGE_DIR` is the exception: its
/// `pml4_phys` is CR3's own frame, recorded by [`init_paging`], and the
/// kernel-half mapping functions in this module write into that tree.
#[repr(C)]
pub struct ProcessPageDir {
    /// The PML4 frame this descriptor accounts for. The descent roots on
    /// this physical address; nothing caches a pointer to the frame,
    /// because every page-table access goes through the HHDM one entry
    /// at a time (see `page_table_defs`).
    pub pml4_phys: PhysAddr,
    pub ref_count: u32,
    pub process_id: u32,
    /// Intrusive next-link. `KernelSync` wraps the raw pointer so the
    /// surrounding struct auto-derives `Send + Sync`; a descriptor is
    /// owned by one process and reached only through the per-process
    /// `SpinLock` in `process_vm.rs`, so the link carries no lock of its
    /// own.
    pub next: slopos_ostd::sync::KernelSync<*mut ProcessPageDir>,
    pub kernel_mapping_gen: u64,
    pub mm_ctx_id: crate::mmu::MmContextId,
}

impl ProcessPageDir {
    /// Build a fresh per-process page-directory descriptor with default
    /// fields. The caller writes the result into a freshly `kmalloc`'d
    /// slot via [`init_in_kmalloc_slot`](Self::init_in_kmalloc_slot).
    pub fn new(pml4_phys: PhysAddr, process_id: u32, mm_ctx_id: crate::mmu::MmContextId) -> Self {
        Self {
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
    /// `with_ref` helper so this site stays in safe Rust.
    #[inline]
    pub fn pml4_phys_from_raw(page_dir: *mut ProcessPageDir) -> PhysAddr {
        if page_dir.is_null() {
            return PhysAddr::NULL;
        }
        slopos_ostd::util::ptr_buf::with_ref::<ProcessPageDir, _>(page_dir, |dir| dir.pml4_phys)
    }

    /// Initialise the freshly `kmalloc`'d slot at `dst` with `init`.
    /// Replaces the bare `core::ptr::write(dst, init)` callers used to
    /// write so that the only `unsafe` is the OSTD helper internal to
    /// `slopos_ostd::util::ptr_buf::init_slot`.
    #[inline]
    pub fn init_in_kmalloc_slot(dst: *mut ProcessPageDir, init: ProcessPageDir) {
        slopos_ostd::util::ptr_buf::init_slot(dst, init);
    }
}

static KERNEL_PAGE_DIR: SyncUnsafeCell<ProcessPageDir> = SyncUnsafeCell::new(ProcessPageDir {
    pml4_phys: PhysAddr::NULL,
    ref_count: 1,
    process_id: 0,
    next: slopos_ostd::sync::KernelSync::new(ptr::null_mut()),
    kernel_mapping_gen: 0,
    mm_ctx_id: crate::mmu::MmContextId::INVALID,
});

/// Allocate a zeroed page-table frame.
///
/// `alloc_kernel_page` already hands back a zeroed frame — its typed
/// allocation forces zeroing regardless of the runtime options — so the
/// explicit clear is defence in depth rather than a load-bearing step.
/// It is kept because the descent links a table into the live tree before
/// it has written every entry, and that should not rest on an allocator
/// contract alone.
fn alloc_page_table() -> Option<PhysAddr> {
    let phys = alloc_kernel_page();
    if phys.is_null() {
        return None;
    }
    zero_table_at(phys);
    Some(phys)
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

/// Demote a 1 GiB leaf into a PD of 2 MiB leaves covering the same
/// range. Returns the new PD's frame and the link the caller publishes in
/// place of the leaf — the parent entry arrives by value, so nothing here
/// holds a reference into the parent table.
fn split_pdpt_huge(pdpt_entry: PageTableEntry) -> Option<(PhysAddr, PageTableEntry)> {
    debug_assert!(pdpt_entry.is_present() && pdpt_entry.is_huge());

    let huge_phys = pdpt_entry.address();
    let huge_flags = pdpt_entry.flags();
    let pd_phys = alloc_page_table()?;

    for i in 0..PAGE_TABLE_ENTRIES {
        let phys = huge_phys.offset(i as u64 * PAGE_SIZE_2MB);
        set_entry_at(
            pd_phys,
            i,
            PageTableEntry::new(phys, huge_flags | PageFlags::HUGE),
        );
    }

    Some((
        pd_phys,
        PageTableEntry::new(pd_phys, table_flags_from_leaf(huge_flags)),
    ))
}

/// Demote a 2 MiB leaf into a PT of 4 KiB leaves covering the same
/// range. Same by-value shape as [`split_pdpt_huge`].
fn split_pd_huge(pd_entry: PageTableEntry) -> Option<(PhysAddr, PageTableEntry)> {
    debug_assert!(pd_entry.is_present() && pd_entry.is_huge());

    let huge_phys = pd_entry.address();
    let mut huge_flags = pd_entry.flags();
    huge_flags.remove(PageFlags::HUGE);
    let pt_phys = alloc_page_table()?;

    for i in 0..PAGE_TABLE_ENTRIES {
        let phys = huge_phys.offset(i as u64 * PAGE_SIZE_4KB);
        set_entry_at(pt_phys, i, PageTableEntry::new(phys, huge_flags));
    }

    Some((
        pt_phys,
        PageTableEntry::new(pt_phys, table_flags_from_leaf(huge_flags)),
    ))
}

/// The PML4 frame backing the kernel-half tree these functions write.
#[inline]
fn kernel_pml4_phys() -> PhysAddr {
    ProcessPageDir::pml4_phys_from_raw(KERNEL_PAGE_DIR.get())
}

/// `entry` with USER set, for promoting an intermediate a user mapping
/// now has to pass through.
#[inline]
fn promoted_to_user(entry: PageTableEntry) -> PageTableEntry {
    let mut promoted = entry;
    promoted.add_flags(PageFlags::USER);
    promoted
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

pub fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    let pml4_phys = kernel_pml4_phys();
    if pml4_phys.is_null() {
        return PhysAddr::NULL;
    }
    match walk_phys(pml4_phys, vaddr) {
        Ok(result) => result.phys_addr,
        Err(_) => PhysAddr::NULL,
    }
}

/// Map a 4 KiB page under the PML4 at `pml4_phys`, demoting any huge
/// leaf in the way.
///
/// The descent carries `(PhysAddr, usize)` per level and moves entries by
/// value, so no reference into a page-table frame exists at any point.
pub(crate) fn map_page_4kb_in(
    pml4_phys: PhysAddr,
    vaddr: VirtAddr,
    paddr: PhysAddr,
    flags: u64,
) -> c_int {
    if !vaddr.is_aligned(PAGE_SIZE_4KB) || !paddr.is_aligned(PAGE_SIZE_4KB) {
        return -1;
    }
    if pml4_phys.is_null() {
        return -1;
    }

    let flags = PageFlags::from_bits_truncate(flags);
    let user_mapping = flags.contains(PageFlags::USER) && is_user_address(vaddr);
    let inter_flags = intermediate_flags(user_mapping);

    let pml4_idx = PageTableLevel::Four.index_of(vaddr);
    let pdpt_idx = PageTableLevel::Three.index_of(vaddr);
    let pd_idx = PageTableLevel::Two.index_of(vaddr);
    let pt_idx = PageTableLevel::One.index_of(vaddr);

    let pml4_entry = entry_at(pml4_phys, pml4_idx);
    let pdpt_phys = if !pml4_entry.is_present() {
        let Some(phys) = alloc_page_table() else {
            klog_info!(
                "Paging: Failed to allocate PDPT for vaddr 0x{:x}",
                vaddr.as_u64()
            );
            return -1;
        };
        set_entry_at(pml4_phys, pml4_idx, PageTableEntry::new(phys, inter_flags));
        phys
    } else {
        if pml4_entry.is_huge() {
            return -1;
        }
        if user_mapping && !pml4_entry.is_user() {
            set_entry_at(pml4_phys, pml4_idx, promoted_to_user(pml4_entry));
        }
        let child = pml4_entry.address();
        if child.is_null() {
            return -1;
        }
        child
    };

    let pdpt_entry = entry_at(pdpt_phys, pdpt_idx);
    let pd_phys = if !pdpt_entry.is_present() {
        let Some(phys) = alloc_page_table() else {
            klog_info!(
                "Paging: Failed to allocate PD for vaddr 0x{:x}",
                vaddr.as_u64()
            );
            return -1;
        };
        set_entry_at(pdpt_phys, pdpt_idx, PageTableEntry::new(phys, inter_flags));
        phys
    } else if pdpt_entry.is_huge() {
        let Some((phys, link)) = split_pdpt_huge(pdpt_entry) else {
            return -1;
        };
        set_entry_at(pdpt_phys, pdpt_idx, link);
        phys
    } else {
        if user_mapping && !pdpt_entry.is_user() {
            set_entry_at(pdpt_phys, pdpt_idx, promoted_to_user(pdpt_entry));
        }
        let child = pdpt_entry.address();
        if child.is_null() {
            return -1;
        }
        child
    };

    let pd_entry = entry_at(pd_phys, pd_idx);
    let pt_phys = if !pd_entry.is_present() {
        let Some(phys) = alloc_page_table() else {
            klog_info!(
                "Paging: Failed to allocate PT for vaddr 0x{:x}",
                vaddr.as_u64()
            );
            return -1;
        };
        set_entry_at(pd_phys, pd_idx, PageTableEntry::new(phys, inter_flags));
        phys
    } else if pd_entry.is_huge() {
        let Some((phys, link)) = split_pd_huge(pd_entry) else {
            return -1;
        };
        set_entry_at(pd_phys, pd_idx, link);
        phys
    } else {
        if user_mapping && !pd_entry.is_user() {
            set_entry_at(pd_phys, pd_idx, promoted_to_user(pd_entry));
        }
        let child = pd_entry.address();
        if child.is_null() {
            return -1;
        }
        child
    };

    let displaced_leaf = entry_at(pt_phys, pt_idx);
    set_entry_at(
        pt_phys,
        pt_idx,
        PageTableEntry::new(
            paddr,
            leaf_flags_with_global(flags, user_mapping) | PageFlags::PRESENT,
        ),
    );

    if displaced_leaf.is_present() {
        // Order is load-bearing: publish the new entry, invalidate the
        // displaced translation machine-wide, and only then hand the frame
        // it named back to the allocator. A CPU still holding the old
        // writable entry would otherwise write into a page the buddy has
        // already reissued.
        flush_kernel_page_after_mod(vaddr);
        let displaced = displaced_leaf.address();
        if !displaced.is_null() && displaced != paddr {
            free_page_frame(displaced);
        }
    }
    0
}

pub fn map_page_4kb(vaddr: VirtAddr, paddr: PhysAddr, flags: u64) -> c_int {
    map_page_4kb_in(kernel_pml4_phys(), vaddr, paddr, flags)
}

/// Unmap `vaddr` under the PML4 at `pml4_phys` and release whatever
/// intermediate tables the cleared leaf emptied. Returns the physical
/// address the leaf named, or NULL if nothing was mapped.
///
/// The descent records `(table, index)` per level on the way down and
/// prunes off that array on the way back up, so at no point does a
/// reference into a page-table frame exist.
pub(crate) fn unmap_page_4kb_in(pml4_phys: PhysAddr, vaddr: VirtAddr) -> PhysAddr {
    if pml4_phys.is_null() {
        return PhysAddr::NULL;
    }

    let mut path = [(PhysAddr::NULL, 0usize); PAGE_TABLE_LEVELS];
    let mut depth = 0usize;

    path[depth] = (pml4_phys, PageTableLevel::Four.index_of(vaddr));
    depth += 1;
    let pml4_entry = entry_at(path[0].0, path[0].1);
    if !pml4_entry.is_present() || pml4_entry.address().is_null() {
        return PhysAddr::NULL;
    }

    path[depth] = (pml4_entry.address(), PageTableLevel::Three.index_of(vaddr));
    depth += 1;
    let pdpt_entry = entry_at(path[1].0, path[1].1);
    if !pdpt_entry.is_present() {
        return PhysAddr::NULL;
    }

    let unmapped_phys = if pdpt_entry.is_huge() {
        set_entry_at(path[1].0, path[1].1, PageTableEntry::EMPTY);
        flush_kernel_page_after_mod(vaddr);
        pdpt_entry.address()
    } else if pdpt_entry.address().is_null() {
        return PhysAddr::NULL;
    } else {
        path[depth] = (pdpt_entry.address(), PageTableLevel::Two.index_of(vaddr));
        depth += 1;
        let pd_entry = entry_at(path[2].0, path[2].1);
        if !pd_entry.is_present() {
            return PhysAddr::NULL;
        }

        if pd_entry.is_huge() {
            set_entry_at(path[2].0, path[2].1, PageTableEntry::EMPTY);
            flush_kernel_page_after_mod(vaddr);
            pd_entry.address()
        } else if pd_entry.address().is_null() {
            return PhysAddr::NULL;
        } else {
            path[depth] = (pd_entry.address(), PageTableLevel::One.index_of(vaddr));
            depth += 1;
            let pt_entry = entry_at(path[3].0, path[3].1);
            if pt_entry.is_present() {
                set_entry_at(path[3].0, path[3].1, PageTableEntry::EMPTY);
                flush_kernel_page_after_mod(vaddr);
                pt_entry.address()
            } else {
                PhysAddr::NULL
            }
        }
    };

    prune_empty_tables(&path, depth);
    unmapped_phys
}

/// Release the intermediate tables the cleared leaf emptied.
///
/// `path[k]` is the table entered at level `k`, PML4 first, paired with
/// the index taken out of it — so `path[k]`'s table is exactly the child
/// of `path[k - 1]`'s entry, and one step releases a child while clearing
/// its single parent link. A non-empty child ends the walk: its parent
/// still holds the present entry that named it, so no ancestor can be
/// empty either.
///
/// Two conditions make the release safe, and both are conditions rather
/// than proofs.
///
/// The first is invalidation. Freeing a page-table frame issues no
/// invalidation of its own. It does not need to, because every leaf this
/// subtree ever translated was cleared by an `unmap_page_4kb_in` that
/// issued `invlpg` for that exact linear address on this CPU and, under
/// SMP, on every other CPU before returning — and `invlpg` drops the
/// paging-structure-cache entries a walk of that address would use, not
/// just its final TLB entry. An empty table is therefore one whose every
/// covered address has already been invalidated machine-wide. Batching
/// those flushes, deferring one past this point, or loosening the
/// emptiness test breaks that silently.
///
/// The second is single ownership. Nothing serialises this descent, and
/// `KERNEL_PAGE_DIR` carries no lock — one cannot be added, because
/// `alloc_page_table` reaches the buddy whose reuse path drains other
/// CPUs, and that drain under a lock is a deadlock. `unlink_child` is
/// what makes the release single-owner: two CPUs clearing the last leaf
/// under one table can both find it empty, and only the one that wins the
/// exchange clearing the parent link may hand the frame back.
fn prune_empty_tables(path: &[(PhysAddr, usize); PAGE_TABLE_LEVELS], depth: usize) {
    let mut level = depth;
    while level > 1 {
        let (child_phys, _) = path[level - 1];
        if !table_empty_at(child_phys) {
            break;
        }
        let (parent_phys, parent_idx) = path[level - 2];
        if !unlink_child(parent_phys, parent_idx, child_phys) {
            break;
        }
        free_page_frame(child_phys);
        level -= 1;
    }
}

pub fn unmap_page(vaddr: VirtAddr) -> PhysAddr {
    unmap_page_4kb_in(kernel_pml4_phys(), vaddr)
}

pub fn paging_get_kernel_directory() -> *mut ProcessPageDir {
    KERNEL_PAGE_DIR.get()
}

pub fn init_paging() {
    let cr3 = get_cr3();
    // Every page-table frame is reached through the HHDM, so a CR3 the
    // HHDM cannot translate is a tree these functions cannot walk.
    if cr3.to_virt().is_null() {
        panic!("Failed to translate kernel PML4 physical address");
    }
    // Scoped so the exclusive borrow ends before `virt_to_phys` below,
    // which reads the same static.
    slopos_ostd::util::ptr_buf::with_ref_mut::<ProcessPageDir, _>(KERNEL_PAGE_DIR.get(), |dir| {
        dir.pml4_phys = cr3;
    });

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

pub fn is_mapped(vaddr: VirtAddr) -> c_int {
    (!virt_to_phys(vaddr).is_null()) as c_int
}

pub fn get_page_size(vaddr: VirtAddr) -> u64 {
    let pml4_phys = kernel_pml4_phys();
    if pml4_phys.is_null() {
        return 0;
    }
    match walk_phys(pml4_phys, vaddr) {
        Ok(result) => result.page_size,
        Err(_) => 0,
    }
}
