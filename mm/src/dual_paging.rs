//! Dual-write helpers that drive both the legacy `mm/src/paging`
//! `*_in_dir` surface and the OSTD `VmSpace` cursor over the same
//! virtual address range. Used during the framekernel migration
//! window: while the kernel still installs the legacy PML4 in CR3,
//! every map / unmap / protect must propagate to OSTD's parallel
//! page-table tree so the eventual reader-flip in [`process_vm`]'s
//! `process_vm_get_cr3_phys` lands on a content-equivalent OSTD
//! PML4.
//!
//! The legacy half remains the sole owner of the underlying physical
//! frame for the duration of the dual-write window; the OSTD half
//! wraps each user-leaf paddr through
//! [`UFrame::wrap_static`](slopos_ostd::mm::uframe::UFrame::wrap_static)
//! so its leaked-into-PTE ref is a no-op on Drop and never
//! double-frees against the legacy
//! [`free_page_frame`](crate::page_alloc::free_page_frame).
//!
//! All public helpers here delete with the rest of the legacy paging
//! surface; the only OSTD-side calls survive.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::frame::{AnonymousMeta, Paddr, RingMeta};
use slopos_ostd::mm::page_property::{CachePolicy, PageProperty};
use slopos_ostd::mm::page_size::Size4Kb;
use slopos_ostd::mm::page_table::PteFlags;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::{MapError, VmSpace};

use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};

/// Bit shift for the AVL software-bits field (PTE bits 9..=11). The
/// legacy [`PageFlags::COW`] sits at bit 9, i.e. the low bit of
/// `PageProperty::software`.
const SOFTWARE_BITS_SHIFT: u32 = 9;

/// Convert a legacy `PageFlags` bitfield (passed as `u64`) into an
/// OSTD `PageProperty`. Round-trips through the AVL software bits
/// (PTE 9..=11).
pub fn page_flags_to_property(flags: u64) -> PageProperty {
    let f = PageFlags::from_bits_truncate(flags);
    let cache_policy = if f.contains(PageFlags::WRITE_THROUGH) {
        CachePolicy::WriteCombining
    } else if f.contains(PageFlags::CACHE_DISABLE) {
        CachePolicy::Uncacheable
    } else {
        CachePolicy::WriteBack
    };
    let software = ((flags & PteFlags::SOFTWARE_BITS_MASK) >> SOFTWARE_BITS_SHIFT) as u8;
    PageProperty {
        read: f.contains(PageFlags::PRESENT),
        write: f.contains(PageFlags::WRITABLE),
        execute: !f.contains(PageFlags::NO_EXECUTE),
        user: f.contains(PageFlags::USER),
        cache_policy,
        global: f.contains(PageFlags::GLOBAL),
        software,
    }
}

/// Convert an OSTD `PageProperty` back into legacy `PageFlags`
/// bits. Inverse of [`page_flags_to_property`].
pub fn property_to_page_flags(prop: PageProperty) -> PageFlags {
    let mut bits = 0u64;
    if prop.read {
        bits |= PageFlags::PRESENT.bits();
    }
    if prop.write {
        bits |= PageFlags::WRITABLE.bits();
    }
    if !prop.execute {
        bits |= PageFlags::NO_EXECUTE.bits();
    }
    if prop.user {
        bits |= PageFlags::USER.bits();
    }
    if prop.global {
        bits |= PageFlags::GLOBAL.bits();
    }
    match prop.cache_policy {
        CachePolicy::WriteBack => {}
        CachePolicy::WriteCombining => bits |= PageFlags::WRITE_THROUGH.bits(),
        CachePolicy::Uncacheable => bits |= PageFlags::CACHE_DISABLE.bits(),
    }
    bits |= (prop.software as u64) << SOFTWARE_BITS_SHIFT;
    PageFlags::from_bits_truncate(bits)
}

/// Helper: borrow the inner `VmSpace` from a sole-owned `KArc<VmSpace>`.
/// Panics if any other clone exists — the dual-write contract is that
/// the per-process [`SpinLock<ProcessVmInner>`] holds the only ref.
#[inline]
fn vm_space_get_mut(vm_space: &mut KArc<VmSpace>) -> &mut VmSpace {
    KArc::get_mut(vm_space).expect(
        "dual_paging: KArc<VmSpace> must be sole-owned (no concurrent cloners) \
         while a cursor mutation is in flight",
    )
}

/// Map a 4 KiB user page into `vm_space` at `va`, pointing at the
/// already-allocated physical page `pa`. The legacy
/// [`map_page_4kb_in_dir`](crate::paging::map_page_4kb_in_dir) caller
/// is responsible for the legacy half; this helper drives only the
/// OSTD half so existing legacy callers keep their semantics during
/// the dual-write window.
///
/// # Errors
///
/// Returns `MapError::Overlap` if a leaf is already present at `va`,
/// `MapError::IntermediateAllocFailed` on intermediate page-table
/// allocation failure, or any other variant the cursor surfaces.
pub fn ostd_map_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
    pa: PhysAddr,
    flags: u64,
) -> Result<(), MapError> {
    let prop = page_flags_to_property(flags);
    let frame = UFrame::<AnonymousMeta>::wrap_user_paddr(Paddr::new(pa.as_u64()))
        .map_err(|_| MapError::PathCorrupt)?;
    let vs = vm_space_get_mut(vm_space);
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    cursor.map::<Size4Kb, AnonymousMeta>(frame, prop)
}

/// Unmap a 4 KiB user page from `vm_space` at `va`. The returned
/// `UFrame` is dropped immediately — its `Drop` decrements the
/// META_SLOTS ref-count and, when it hits zero, returns the page to
/// the registered [`FrameAlloc`].
///
/// Returns `Ok(true)` if a leaf was present and unmapped, `Ok(false)`
/// if the leaf was already absent, or an error from the cursor.
pub fn ostd_unmap_4kb_user(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<bool, MapError> {
    let vs = vm_space_get_mut(vm_space);
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    Ok(cursor
        .unmap::<Size4Kb, AnonymousMeta>()
        .map(|opt| opt.is_some())?)
}

/// Map a 4 KiB SlopRing page (`Frame<RingMeta>`) into `vm_space` at
/// `va`. The `RingMeta` slot at `pa` must already be live (the ring
/// object holds the first ref); this bumps it via `from_in_use` and
/// leaks that second ref into the user PTE (SLOPRING § 5.1). Because
/// the frame's refcount now reflects both the ring object and the PTE,
/// the page is freed only once *both* drop their ref — so a mapping
/// that outlives the ring fd cannot UAF.
pub fn ostd_map_ring_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
    pa: PhysAddr,
    flags: u64,
) -> Result<(), MapError> {
    let prop = page_flags_to_property(flags);
    let frame = UFrame::<RingMeta>::from_in_use(Paddr::new(pa.as_u64()))
        .map_err(|_| MapError::PathCorrupt)?;
    let vs = vm_space_get_mut(vm_space);
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    cursor.map::<Size4Kb, RingMeta>(frame, prop)
}

/// Unmap a 4 KiB SlopRing page from `vm_space` at `va`. The returned
/// `UFrame<RingMeta>` is dropped immediately, releasing the PTE's ref;
/// the underlying frame survives until the ring object also drops its
/// ref (and vice-versa). Returns `Ok(true)` if a leaf was present.
pub fn ostd_unmap_ring_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
) -> Result<bool, MapError> {
    let vs = vm_space_get_mut(vm_space);
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    Ok(cursor
        .unmap::<Size4Kb, RingMeta>()
        .map(|opt| opt.is_some())?)
}

/// Apply the writable / no-execute bits from `new_flags` to every
/// 4 KiB leaf in `[start, end)` that is currently present. Skips
/// huge leaves (slopos `paging_update_range_protection` does the
/// same — it only touches PT-level entries).
pub fn ostd_protect_range_4kb(
    vm_space: &mut KArc<VmSpace>,
    start: VirtAddr,
    end: VirtAddr,
    new_flags: PageFlags,
) -> Result<(), MapError> {
    if end.as_u64() <= start.as_u64() {
        return Ok(());
    }
    let vs = vm_space_get_mut(vm_space);
    let mut cursor = vs.cursor_mut(start..end)?;
    let mut va = start.as_u64();
    while va < end.as_u64() {
        let entry = cursor.query();
        let cur = match entry {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        if cur.paddr.is_some() {
            // Only mutate at the cursor's actual leaf size if it's a
            // 4 KiB leaf. Huge leaves are skipped — legacy parity.
            if cur.level == slopos_ostd::mm::page_table::PageTableLevel::One {
                let mut prop = cur.property;
                prop.write = new_flags.contains(PageFlags::WRITABLE);
                prop.execute = !new_flags.contains(PageFlags::NO_EXECUTE);
                cursor.protect::<Size4Kb>(prop)?;
            }
        }
        va = va.wrapping_add(PAGE_SIZE_4KB);
        cursor.advance(PAGE_SIZE_4KB)?;
    }
    Ok(())
}

/// Set the `USER` bit on every 4 KiB leaf in `[start, end)` that is
/// currently present, plus toggle `WRITABLE` according to
/// `writable`. Mirrors slopos's `paging_mark_range_user`.
pub fn ostd_mark_range_user_4kb(
    vm_space: &mut KArc<VmSpace>,
    start: VirtAddr,
    end: VirtAddr,
    writable: bool,
) -> Result<(), MapError> {
    if end.as_u64() <= start.as_u64() {
        return Ok(());
    }
    let vs = vm_space_get_mut(vm_space);
    let mut cursor = vs.cursor_mut(start..end)?;
    let mut va = start.as_u64();
    while va < end.as_u64() {
        let entry = cursor.query();
        let cur = match entry {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        if cur.paddr.is_some() && cur.level == slopos_ostd::mm::page_table::PageTableLevel::One {
            let mut prop = cur.property;
            prop.user = true;
            prop.write = writable;
            cursor.protect::<Size4Kb>(prop)?;
        }
        va = va.wrapping_add(PAGE_SIZE_4KB);
        cursor.advance(PAGE_SIZE_4KB)?;
    }
    Ok(())
}

/// Mark a single 4 KiB user page as copy-on-write: clear `WRITABLE`
/// and set the slopos COW software bit (PTE bit 9). Mirrors
/// `paging_mark_cow`.
pub fn ostd_mark_cow_4kb(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<(), MapError> {
    let vs = vm_space_get_mut(vm_space);
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    let cur = match cursor.query() {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    if cur.paddr.is_some() && cur.level == slopos_ostd::mm::page_table::PageTableLevel::One {
        let mut prop = cur.property;
        prop.write = false;
        // Bit 0 of `software` ↔ PTE bit 9 ↔ legacy `PageFlags::COW`.
        prop.software |= 0b001;
        cursor.protect::<Size4Kb>(prop)?;
    }
    Ok(())
}

/// Resolve a copy-on-write page for the single-ref case: set
/// `WRITABLE` and clear the slopos COW software bit. Mirrors
/// `paging_resolve_cow`'s flag mutation. Returns `Ok(true)` if a
/// 4 KiB leaf was present and updated, `Ok(false)` otherwise.
pub fn ostd_resolve_cow_4kb(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<bool, MapError> {
    let vs = vm_space_get_mut(vm_space);
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    let cur = match cursor.query() {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };
    if cur.paddr.is_some() && cur.level == slopos_ostd::mm::page_table::PageTableLevel::One {
        let mut prop = cur.property;
        prop.write = true;
        prop.software &= !0b001;
        cursor.protect::<Size4Kb>(prop)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Read-side query: return the legacy `PageFlags` snapshot of the
/// 4 KiB leaf at `va`, computed from the OSTD cursor entry. Returns
/// `None` if no leaf is present at the requested level.
pub fn ostd_get_pte_flags_4kb(vm_space: &KArc<VmSpace>, va: VirtAddr) -> Option<PageFlags> {
    // Read-only borrow — Cursor takes `&VmSpace`. KArc dereferences
    // to `&VmSpace` directly without get_mut.
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let cursor = match vm_space.cursor(range) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let cur = cursor.query().ok()?;
    if cur.paddr.is_none() {
        return None;
    }
    if cur.level != slopos_ostd::mm::page_table::PageTableLevel::One {
        // Huge leaf — legacy `paging_get_pte_flags` only walks 4 KiB
        // entries. Return None for parity.
        return None;
    }
    Some(property_to_page_flags(cur.property))
}

/// Read-side query: is the 4 KiB leaf at `va` mapped AND
/// user-accessible? Mirrors `paging_is_user_accessible`'s semantics —
/// kernel-half pages (USER bit clear) return `false` even though
/// they're mapped.
pub fn ostd_is_user_accessible_4kb(vm_space: &KArc<VmSpace>, va: VirtAddr) -> bool {
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let cursor = match vm_space.cursor(range) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let cur = match cursor.query() {
        Ok(e) => e,
        Err(_) => return false,
    };
    cur.paddr.is_some()
        && cur.level == slopos_ostd::mm::page_table::PageTableLevel::One
        && cur.property.user
}

/// Read-side query: return the physical address backing the 4 KiB
/// user leaf at `va`, or `PhysAddr::null()` if no leaf is present.
/// Mirrors `virt_to_phys_in_dir`'s 4 KiB output (huge leaves return
/// null for parity with legacy).
pub fn ostd_virt_to_phys_4kb(vm_space: &KArc<VmSpace>, va: VirtAddr) -> PhysAddr {
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let cursor = match vm_space.cursor(range) {
        Ok(c) => c,
        Err(_) => return PhysAddr::new(0),
    };
    let cur = match cursor.query() {
        Ok(e) => e,
        Err(_) => return PhysAddr::new(0),
    };
    match cur.paddr {
        Some(p) if cur.level == slopos_ostd::mm::page_table::PageTableLevel::One => {
            // Add page offset back in for parity with `virt_to_phys_in_dir`,
            // which returns the exact byte address.
            let off = va.as_u64() & (PAGE_SIZE_4KB - 1);
            PhysAddr::new(p.as_u64() | off)
        }
        _ => PhysAddr::new(0),
    }
}
