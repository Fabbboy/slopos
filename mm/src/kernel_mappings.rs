//! Kernel-half VA mapping helpers built on the OSTD `VmSpace` cursor. Every
//! kernel-half page-table write goes through here; `kernel_vm_space().lock()`
//! is what mints the `&mut VmSpace`, so the master has exactly one writer at a
//! time, while reads take the shared `cursor()`.

use core::ffi::c_int;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_kernel_services::kernel_vm_space::kernel_vm_space;
use slopos_ostd::mm::frame::{Frame, KernelMeta, Paddr};
use slopos_ostd::mm::page_property::PageProperty;
use slopos_ostd::mm::page_size::{Size1Gb, Size2Mb, Size4Kb};
use slopos_ostd::mm::page_table::PageTableLevel;

use crate::paging_defs::{PAGE_SIZE_1GB, PAGE_SIZE_2MB, PAGE_SIZE_4KB, PageFlags};
use crate::user_mappings::{page_flags_to_property, property_to_page_flags};

/// Lowest canonical higher-half address — PML4 index 256.
const KERNEL_HALF_START: u64 = 0xFFFF_8000_0000_0000;

/// Kernel-half leaves carry `GLOBAL`: they translate identically in every
/// address space, so a CR3 reload must not evict them. CR4.PGE is enabled at
/// boot priority memory/1, ahead of the first kernel mapping. A leaf below the
/// higher half is a firmware alias and stays non-global — a global entry there
/// would survive into a user address space mapping the same address.
fn kernel_leaf_property(va: VirtAddr, flags: u64) -> PageProperty {
    let mut prop = page_flags_to_property(flags);
    if va.as_u64() >= KERNEL_HALF_START {
        prop.global = true;
    }
    prop
}

/// Map a 4 KiB kernel-half page at `va` to physical `pa`. Returns 0, or -1 on
/// any cursor error (alignment, already-mapped, intermediate alloc fail).
///
/// **Takes ownership of `pa`.** On success the leaf owns the page and
/// [`kernel_unmap_4kb`] hands it back; on failure it returns to the frame
/// allocator. Either way the caller must not free it again, and `pa` must be
/// allocator-owned RAM — use [`kernel_map_io_4kb`] for anything else.
pub fn kernel_map_4kb(va: VirtAddr, pa: PhysAddr, flags: u64) -> c_int {
    if !va.is_aligned(PAGE_SIZE_4KB) || !pa.is_aligned(PAGE_SIZE_4KB) {
        return -1;
    }
    let frame = match Frame::<KernelMeta>::from_unused(Paddr::new(pa.as_u64()), KernelMeta) {
        Ok(f) => f,
        Err(_) => return -1,
    };
    kernel_map_4kb_frame(va, frame, flags)
}

/// [`kernel_map_4kb`] for a caller that already holds the frame: the cursor
/// consumes it and leaks its single reference into the leaf entry, so the
/// page's owner is the page table rather than nobody.
pub fn kernel_map_4kb_frame(va: VirtAddr, frame: Frame<KernelMeta>, flags: u64) -> c_int {
    if !va.is_aligned(PAGE_SIZE_4KB) {
        return -1;
    }
    let prop = kernel_leaf_property(va, flags);
    let mut guard = kernel_vm_space().lock();
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cur = match guard.cursor_mut(range) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    // No TLB invalidation on the map side: the cursor refuses to overwrite a
    // present leaf, so every kernel-half map takes the entry from not-present
    // to present, and x86-64 architecturally caches no translation for a
    // not-present entry (SDM Vol. 3A §4.10.2.3).
    match cur.map_kernel::<Size4Kb, KernelMeta>(frame, prop) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Map a 4 KiB kernel-half page at `va` over physical memory the kernel does
/// not own — a device MMIO aperture, or a firmware region the buddy allocator
/// must never see.
///
/// No `MetaSlot` is touched: `META_SLOTS` is sized by the highest RAM frame and
/// excludes device apertures, so wrapping such a paddr as a `Frame` either
/// fails outright or aliases a slot naming unrelated memory. The leaf records
/// that it owns nothing, and the unmap path reads that back.
///
/// `va` must be in the higher half, or else be the identity alias of `pa` —
/// the UEFI runtime-services window, whose pages `ResetSystem` needs reachable
/// at their physical address because firmware keeps physical pointers into its
/// own runtime code.
pub fn kernel_map_io_4kb(va: VirtAddr, pa: PhysAddr, flags: u64) -> c_int {
    if !va.is_aligned(PAGE_SIZE_4KB) || !pa.is_aligned(PAGE_SIZE_4KB) {
        return -1;
    }
    if va.as_u64() < KERNEL_HALF_START && va.as_u64() != pa.as_u64() {
        return -1;
    }
    let prop = kernel_leaf_property(va, flags);
    let mut guard = kernel_vm_space().lock();
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cur = match guard.cursor_mut(range) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    match cur.map_io::<Size4Kb>(Paddr::new(pa.as_u64()), prop) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Unmap the 4 KiB kernel-half leaf at `va`, returning the physical address it
/// named, or `PhysAddr::NULL` if nothing was present or the leaf owned no
/// frame. The page is **freed** here; callers must not free it again.
///
/// Intermediate page tables are not pruned: a table linked into the kernel half
/// stays linked for the lifetime of the kernel, which is what lets every
/// address space copy the kernel half once at construction and never
/// resynchronise.
pub fn kernel_unmap_4kb(va: VirtAddr) -> PhysAddr {
    if !va.is_aligned(PAGE_SIZE_4KB) {
        return PhysAddr::NULL;
    }
    // The frame outlives this block so its Drop, which reaches the buddy
    // allocator, runs with the KERNEL_VM_SPACE guard already released.
    let unmapped = {
        let mut guard = kernel_vm_space().lock();
        let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
        let mut cur = match guard.cursor_mut(range) {
            Ok(c) => c,
            Err(_) => return PhysAddr::NULL,
        };
        match cur.unmap_kernel::<Size4Kb, KernelMeta>() {
            Ok(frame) => frame,
            Err(_) => None,
        }
    };
    let Some(frame) = unmapped else {
        return PhysAddr::NULL;
    };
    let pa = PhysAddr::new(frame.paddr().as_u64());
    // Order is load-bearing. The cursor invalidates the cleared leaf on this
    // CPU only, so the shootdown is issued here: after the KERNEL_VM_SPACE
    // guard is released, because it waits for every other CPU to acknowledge
    // and an IRQ-disabling spinlock held across that wait deadlocks the peers;
    // and before the frame goes back, because a CPU still holding the old
    // writable entry would write into a page the allocator has reissued.
    crate::tlb::flush_page(va);
    drop(frame);
    pa
}

/// Translate a kernel-half virtual address to its backing physical address, or
/// `PhysAddr::NULL` if not mapped. The result preserves the page offset of `va`.
pub fn kernel_virt_to_phys(va: VirtAddr) -> PhysAddr {
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let guard = kernel_vm_space().lock();
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let cur = match guard.cursor(range) {
        Ok(c) => c,
        Err(_) => return PhysAddr::NULL,
    };
    let entry = match cur.query() {
        Ok(e) => e,
        Err(_) => return PhysAddr::NULL,
    };
    match entry.paddr {
        Some(p) => {
            // A huge leaf is aligned to its own size, so the byte offset must
            // be masked out of `va` at that granularity.
            let leaf_size = match entry.level {
                PageTableLevel::One => PAGE_SIZE_4KB,
                PageTableLevel::Two => PAGE_SIZE_2MB,
                PageTableLevel::Three => PAGE_SIZE_1GB,
                PageTableLevel::Four => PAGE_SIZE_4KB,
            };
            let off = va.as_u64() & (leaf_size - 1);
            PhysAddr::new(p.as_u64() | off)
        }
        None => PhysAddr::NULL,
    }
}

#[allow(dead_code)]
pub fn kernel_is_mapped(va: VirtAddr) -> bool {
    !kernel_virt_to_phys(va).is_null()
}

/// Leaf page size at `va` (4 KiB / 2 MiB / 1 GiB), or 0 if no leaf is present.
#[allow(dead_code)]
pub fn kernel_get_page_size(va: VirtAddr) -> u64 {
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let guard = kernel_vm_space().lock();
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let cur = match guard.cursor(range) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let entry = match cur.query() {
        Ok(e) => e,
        Err(_) => return 0,
    };
    if entry.paddr.is_none() {
        return 0;
    }
    match entry.level {
        PageTableLevel::One => PAGE_SIZE_4KB,
        PageTableLevel::Two => PAGE_SIZE_2MB,
        PageTableLevel::Three => PAGE_SIZE_1GB,
        PageTableLevel::Four => 0,
    }
}

/// Stamp `GLOBAL` onto every present leaf in the kernel half (PML4 indices
/// 256..512), at any leaf size. Idempotent; run by the early boot step that
/// wraps the live kernel-master PML4.
pub fn mark_kernel_global() {
    use slopos_ostd::mm::page_property::PageProperty;

    const KERNEL_HALF_START: u64 = 0xFFFF_8000_0000_0000;
    const KERNEL_HALF_END: u64 = 0xFFFF_FFFF_FFFF_F000;

    let mut guard = kernel_vm_space().lock();
    let range = VirtAddr::new(KERNEL_HALF_START)..VirtAddr::new(KERNEL_HALF_END);
    let mut cur = guard
        .cursor_mut(range)
        .expect("kernel_vm_space cursor over kernel half");

    loop {
        let entry = match cur.query() {
            Ok(e) => e,
            Err(_) => break,
        };

        if entry.paddr.is_some() {
            let new_prop = PageProperty {
                global: true,
                ..entry.property
            };
            let res = match entry.level {
                PageTableLevel::One => cur.protect::<Size4Kb>(new_prop),
                PageTableLevel::Two => cur.protect::<Size2Mb>(new_prop),
                PageTableLevel::Three => cur.protect::<Size1Gb>(new_prop),
                PageTableLevel::Four => Ok(()),
            };
            let _ = res;
        }

        let advance = entry.level.entry_size();
        if cur.advance(advance).is_err() {
            break;
        }
    }
}

#[allow(dead_code)]
pub fn property_to_flags_bits(prop: slopos_ostd::mm::page_property::PageProperty) -> PageFlags {
    property_to_page_flags(prop)
}
