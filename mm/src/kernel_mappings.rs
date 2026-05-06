//! Kernel-half VA mapping helpers built on the OSTD `VmSpace` cursor.
//!
//! Replaces the legacy `mm::paging::{map_page_4kb, unmap_page,
//! virt_to_phys, is_mapped, get_page_size, paging_map_shared_kernel_page}`
//! surface. All mutations go through `kernel_vm_space().lock().cursor_mut(...)`,
//! which acts on the same physical PML4 OSTD wraps via `KERNEL_VM_SPACE`
//! (installed at boot priority 55). Reads use `cursor()` so multiple
//! callers can probe the kernel half concurrently.
//!
//! We use `AnonymousMeta` for kernel-side frames even though the
//! "anonymous" name is user-page-flavoured: the underlying Drop
//! semantics are identical (return to the registered `FrameAlloc`).
//! `KernelMeta` is deliberately not `AnyUFrameMeta` and so cannot
//! flow through the cursor's `map` entry point; AnonymousMeta is the
//! sole `AnyUFrameMeta` impl currently available.

use core::ffi::c_int;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_kernel_services::kernel_vm_space::kernel_vm_space;
use slopos_ostd::mm::frame::{AnonymousMeta, Paddr};
use slopos_ostd::mm::page_size::{Size1Gb, Size2Mb, Size4Kb};
use slopos_ostd::mm::page_table::PageTableLevel;
use slopos_ostd::mm::uframe::UFrame;

use crate::dual_paging::{page_flags_to_property, property_to_page_flags};
use crate::paging_defs::{PAGE_SIZE_1GB, PAGE_SIZE_2MB, PAGE_SIZE_4KB, PageFlags};

/// Map a 4 KiB kernel-half page at `va` to physical `pa` with the
/// given legacy `PageFlags` bits. Returns 0 on success, -1 on any
/// cursor error (alignment, already-mapped, intermediate alloc fail).
pub fn kernel_map_4kb(va: VirtAddr, pa: PhysAddr, flags: u64) -> c_int {
    if !va.is_aligned(PAGE_SIZE_4KB) || !pa.is_aligned(PAGE_SIZE_4KB) {
        return -1;
    }
    let prop = page_flags_to_property(flags);
    let frame = match UFrame::<AnonymousMeta>::from_unused(
        Paddr::new(pa.as_u64()),
        AnonymousMeta::default(),
    ) {
        Ok(f) => f,
        Err(_) => return -1,
    };
    let mut guard = kernel_vm_space().lock();
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cur = match guard.cursor_mut(range) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    match cur.map::<Size4Kb, AnonymousMeta>(frame, prop) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Map a 2 MiB kernel-half huge page at `va` to physical `pa`.
#[allow(dead_code)]
pub fn kernel_map_2mb(va: VirtAddr, pa: PhysAddr, flags: u64) -> c_int {
    if !va.is_aligned(PAGE_SIZE_2MB) || !pa.is_aligned(PAGE_SIZE_2MB) {
        return -1;
    }
    let prop = page_flags_to_property(flags);
    let frame = match UFrame::<AnonymousMeta>::from_unused(
        Paddr::new(pa.as_u64()),
        AnonymousMeta::default(),
    ) {
        Ok(f) => f,
        Err(_) => return -1,
    };
    let mut guard = kernel_vm_space().lock();
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_2MB));
    let mut cur = match guard.cursor_mut(range) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    match cur.map::<Size2Mb, AnonymousMeta>(frame, prop) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Unmap the 4 KiB kernel-half leaf at `va`, returning the freed
/// physical address (or `PhysAddr::NULL` if nothing was present).
/// The Drop on the returned UFrame decrements META_SLOTS — when the
/// count reaches zero the registered allocator deallocs the frame.
pub fn kernel_unmap_4kb(va: VirtAddr) -> PhysAddr {
    if !va.is_aligned(PAGE_SIZE_4KB) {
        return PhysAddr::NULL;
    }
    let mut guard = kernel_vm_space().lock();
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cur = match guard.cursor_mut(range) {
        Ok(c) => c,
        Err(_) => return PhysAddr::NULL,
    };
    match cur.unmap::<Size4Kb, AnonymousMeta>() {
        Ok(Some(uf)) => {
            let pa = PhysAddr::new(uf.paddr().as_u64());
            // UFrame drops here — META_SLOTS decrements; when 0,
            // returns to the buddy via the registered allocator.
            drop(uf);
            pa
        }
        _ => PhysAddr::NULL,
    }
}

/// Translate a kernel-half virtual address to its backing physical
/// address. Returns `PhysAddr::NULL` if not mapped. The result
/// preserves the page offset of `va`, mirroring the legacy
/// `virt_to_phys` semantics.
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
            // Reconstruct the byte-precise address: huge leaves keep
            // their alignment to the leaf size, plus the offset within.
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

/// Probe whether a kernel-half VA is currently mapped.
#[allow(dead_code)]
pub fn kernel_is_mapped(va: VirtAddr) -> bool {
    !kernel_virt_to_phys(va).is_null()
}

/// Return the leaf page size at `va` (4 KiB / 2 MiB / 1 GiB), or 0
/// if no leaf is present.
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

/// Walk the kernel half (PML4 indices 256..512) via the OSTD cursor
/// and stamp `GLOBAL` onto every present leaf — handles 4 KiB / 2 MiB /
/// 1 GiB leaves uniformly. Idempotent. Used by the early boot init
/// step that wraps the live kernel-master PML4.
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

/// Re-export `property_to_page_flags` for callers that need to round-
/// trip OSTD `PageProperty` back to legacy `PageFlags` bits.
#[allow(dead_code)]
pub fn property_to_flags_bits(prop: slopos_ostd::mm::page_property::PageProperty) -> PageFlags {
    property_to_page_flags(prop)
}
