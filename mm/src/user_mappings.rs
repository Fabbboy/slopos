//! User-half mapping helpers, driving a process's OSTD `VmSpace` cursor over a
//! virtual address range: map, unmap, protect, the COW marker pair, and the
//! read-only queries the fault handlers ask. The kernel-half counterpart is
//! [`crate::kernel_mappings`].
//!
//! Every helper takes the address space it operates on, so the caller holds the
//! per-process lock for exactly as long as the cursor is open — see
//! `process_vm::process_vm_with_vm_space`.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::klog_warn;
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::frame::{AnonymousMeta, Paddr, RingMeta};
use slopos_ostd::mm::page_property::{CachePolicy, PageProperty};
use slopos_ostd::mm::page_size::Size4Kb;
use slopos_ostd::mm::page_table::PteFlags;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::{MapError, VmSpace};

use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};

/// Bit shift for the AVL software-bits field (PTE bits 9..=11); legacy
/// [`PageFlags::COW`] sits at bit 9, the low bit of `PageProperty::software`.
const SOFTWARE_BITS_SHIFT: u32 = 9;
/// Only the fault paths can retry a `WouldBlock`; for syscalls the spin is all there is.
const VM_SPACE_MUT_SPINS: usize = 1_000_000;

/// Convert a legacy `PageFlags` bitfield (passed as `u64`) into an OSTD
/// `PageProperty`.
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

/// Inverse of [`page_flags_to_property`].
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

/// Advisory; `KArc::get_mut` is the authoritative check.
#[inline]
pub(crate) fn vm_space_is_exclusive(vm_space: &KArc<VmSpace>) -> bool {
    KArc::strong_count(vm_space) == 1 && KArc::weak_count(vm_space) == 0
}

#[cfg(feature = "test-hooks")]
static VM_SPACE_MUT_SPINS_TAKEN: [core::sync::atomic::AtomicU64; slopos_arch::pcr::MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; slopos_arch::pcr::MAX_CPUS];

#[cfg(feature = "test-hooks")]
pub(crate) fn vm_space_mut_spins_taken(cpu: usize) -> u64 {
    VM_SPACE_MUT_SPINS_TAKEN
        .get(cpu)
        .map_or(0, |c| c.load(core::sync::atomic::Ordering::Relaxed))
}

#[inline]
fn vm_space_get_mut(vm_space: &mut KArc<VmSpace>) -> Result<&mut VmSpace, MapError> {
    let mut spins = 0usize;
    while !vm_space_is_exclusive(vm_space) {
        if spins == VM_SPACE_MUT_SPINS {
            #[cfg(feature = "test-hooks")]
            record_spins(spins);
            return Err(MapError::WouldBlock);
        }
        spins += 1;
        core::hint::spin_loop();
    }
    #[cfg(feature = "test-hooks")]
    record_spins(spins);
    KArc::get_mut(vm_space).ok_or(MapError::WouldBlock)
}

#[cfg(feature = "test-hooks")]
#[inline]
fn record_spins(spins: usize) {
    if spins == 0 {
        return;
    }
    let cpu = slopos_arch::pcr::get_current_cpu();
    if let Some(counter) = VM_SPACE_MUT_SPINS_TAKEN.get(cpu) {
        counter.fetch_add(spins as u64, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Map a 4 KiB user page into `vm_space` at `va`, pointing at the
/// already-allocated physical page `pa`.
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
    let vs = vm_space_get_mut(vm_space)?;
    let prop = page_flags_to_property(flags);
    let pa_ostd = Paddr::new(pa.as_u64());
    let frame = match UFrame::<AnonymousMeta>::wrap_user_paddr(pa_ostd) {
        Ok(f) => f,
        Err(e) => {
            let snap = slopos_ostd::mm::frame::slot_snapshot(pa_ostd);
            let (slots, max_pa, inited) = slopos_ostd::mm::frame::meta_slots_coverage();
            klog_warn!(
                "USERMAP: wrap_user_paddr(Anon) FAILED pa=0x{:x} va=0x{:x} frame_err={:?} \
                 slot_kind={:?} raw_rc=0x{:x} | META_SLOTS inited={} slots={} max_pa=0x{:x}",
                pa.as_u64(),
                va.as_u64(),
                e,
                snap.kind,
                snap.raw_ref_count,
                inited,
                slots,
                max_pa,
            );
            return Err(MapError::PathCorrupt);
        }
    };
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    // TODO(frame-ownership): dropping the returned frame here is the
    // double-free; the caller still frees `pa`. Removed when this takes an
    // owned `UFrame` rather than a `PhysAddr`.
    cursor
        .map::<Size4Kb, AnonymousMeta>(frame, prop)
        .map_err(|(_, e)| e)
}

/// Unmap the 4 KiB user leaf at `va` and map `pa` in its place on one cursor.
/// `unmap` flushes only this CPU, so the caller holds the displaced frame until
/// after a cross-CPU shootdown of `va`.
#[must_use = "dropping the displaced frame before the TLB shootdown frees a page a peer may still translate"]
pub fn ostd_replace_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
    pa: PhysAddr,
    flags: u64,
) -> Result<Option<UFrame<AnonymousMeta>>, MapError> {
    let prop = page_flags_to_property(flags);
    let vs = vm_space_get_mut(vm_space)?;
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    // Unmap before the wrap: a fallible step between could free a page the caller frees.
    let displaced = cursor.unmap::<Size4Kb, AnonymousMeta>()?;
    let frame = UFrame::<AnonymousMeta>::wrap_user_paddr(Paddr::new(pa.as_u64()))
        .map_err(|_| MapError::PathCorrupt)?;
    // TODO(frame-ownership): as `ostd_map_4kb_user`, and this arm also drops
    // `displaced` before its cross-CPU shootdown. Both go with the `replace`
    // cursor primitive.
    cursor
        .map::<Size4Kb, AnonymousMeta>(frame, prop)
        .map_err(|(_, e)| e)?;
    Ok(displaced)
}

/// Unmap a 4 KiB user page from `vm_space` at `va`. The `UFrame` is dropped
/// inline, releasing its META_SLOTS ref and returning the page to the
/// registered [`FrameAlloc`] once that count hits zero.
///
/// `Ok(false)` means the leaf was already absent.
pub fn ostd_unmap_4kb_user(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<bool, MapError> {
    let vs = vm_space_get_mut(vm_space)?;
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    Ok(cursor
        .unmap::<Size4Kb, AnonymousMeta>()
        .map(|opt| opt.is_some())?)
}

/// Unmap a 4 KiB user leaf and return its frame instead of letting it drop
/// inline. The caller must hold the [`UFrame`] until after a cross-CPU TLB
/// shootdown of the range, so a freed frame cannot be reused while a peer CPU
/// still caches a stale translation. The cursor does the local invalidation;
/// the caller's half is to hold the frames, drop the lock, then issue one
/// shootdown for the whole range rather than one per page.
pub fn ostd_unmap_4kb_user_take(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
) -> Result<Option<UFrame<AnonymousMeta>>, MapError> {
    let vs = vm_space_get_mut(vm_space)?;
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    cursor.unmap::<Size4Kb, AnonymousMeta>()
}

/// Map a 4 KiB SlopRing page into `vm_space` at `va`. The `RingMeta` slot at
/// `pa` must already be live — the ring object holds the first ref — and this
/// leaks a second one into the user PTE (SLOPRING § 5.1), so the page is freed
/// only once both drop and a mapping outliving the ring fd cannot UAF.
pub fn ostd_map_ring_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
    pa: PhysAddr,
    flags: u64,
) -> Result<(), MapError> {
    let prop = page_flags_to_property(flags);
    let pa_ostd = Paddr::new(pa.as_u64());
    // A dead `RingMeta` slot and a failed page-table walk both surface as
    // PathCorrupt, so the two failures are logged distinctly.
    let frame = match UFrame::<RingMeta>::from_in_use(pa_ostd) {
        Ok(f) => f,
        Err(e) => {
            let snap = slopos_ostd::mm::frame::slot_snapshot(pa_ostd);
            let (slots, max_pa, inited) = slopos_ostd::mm::frame::meta_slots_coverage();
            klog_warn!(
                "RINGMAP: from_in_use(RingMeta) FAILED pa=0x{:x} va=0x{:x} frame_err={:?} \
                 slot_kind={:?} raw_rc=0x{:x} vtable=0x{:x} | META_SLOTS inited={} \
                 slots={} max_pa=0x{:x}",
                pa.as_u64(),
                va.as_u64(),
                e,
                snap.kind,
                snap.raw_ref_count,
                snap.vtable_addr,
                inited,
                slots,
                max_pa,
            );
            return Err(MapError::PathCorrupt);
        }
    };
    let vs = vm_space_get_mut(vm_space)?;
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    match cursor.map::<Size4Kb, RingMeta>(frame, prop) {
        Ok(()) => Ok(()),
        // Dropping the returned frame is correct here and only here: it is an
        // alias taken by `from_in_use`, so this releases the second ref the
        // failed map would have leaked into the PTE. The ring object's own ref
        // keeps the page alive.
        Err((_, e)) => {
            klog_warn!(
                "RINGMAP: cursor.map(RingMeta) FAILED pa=0x{:x} va=0x{:x} map_err={:?}",
                pa.as_u64(),
                va.as_u64(),
                e,
            );
            Err(e)
        }
    }
}

/// Unmap a 4 KiB SlopRing page from `vm_space` at `va`, releasing the PTE's
/// ref; the frame survives until the ring object drops its own, and vice
/// versa. `Ok(true)` means a leaf was present.
pub fn ostd_unmap_ring_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
) -> Result<bool, MapError> {
    let vs = vm_space_get_mut(vm_space)?;
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    Ok(cursor
        .unmap::<Size4Kb, RingMeta>()
        .map(|opt| opt.is_some())?)
}

/// Apply the writable / no-execute bits from `new_flags` to every 4 KiB leaf
/// in `[start, end)` that is currently present. Huge leaves are skipped.
pub fn ostd_protect_range_4kb(
    vm_space: &mut KArc<VmSpace>,
    start: VirtAddr,
    end: VirtAddr,
    new_flags: PageFlags,
) -> Result<(), MapError> {
    if end.as_u64() <= start.as_u64() {
        return Ok(());
    }
    let vs = vm_space_get_mut(vm_space)?;
    let mut cursor = vs.cursor_mut(start..end)?;
    let mut va = start.as_u64();
    while va < end.as_u64() {
        let entry = cursor.query();
        let cur = match entry {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        if cur.paddr.is_some() {
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

/// Set the `USER` bit on every 4 KiB leaf in `[start, end)` that is currently
/// present, and set `WRITABLE` from `writable`.
pub fn ostd_mark_range_user_4kb(
    vm_space: &mut KArc<VmSpace>,
    start: VirtAddr,
    end: VirtAddr,
    writable: bool,
) -> Result<(), MapError> {
    if end.as_u64() <= start.as_u64() {
        return Ok(());
    }
    let vs = vm_space_get_mut(vm_space)?;
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

/// Mark a single 4 KiB user page as copy-on-write: clear `WRITABLE` and set
/// the COW software bit (PTE bit 9).
pub fn ostd_mark_cow_4kb(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<(), MapError> {
    let vs = vm_space_get_mut(vm_space)?;
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

/// Resolve a copy-on-write page for the single-ref case: set `WRITABLE` and
/// clear the COW software bit. `Ok(true)` if a 4 KiB leaf was present and
/// updated.
pub fn ostd_resolve_cow_4kb(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<bool, MapError> {
    let vs = vm_space_get_mut(vm_space)?;
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

/// Legacy `PageFlags` snapshot of the 4 KiB leaf at `va`, or `None` if no leaf
/// is present at that level.
pub fn ostd_get_pte_flags_4kb(vm_space: &KArc<VmSpace>, va: VirtAddr) -> Option<PageFlags> {
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
        return None;
    }
    Some(property_to_page_flags(cur.property))
}

/// Is the 4 KiB leaf at `va` both mapped and user-accessible? A kernel-half
/// page (USER bit clear) is mapped but answers `false`.
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

/// Physical address backing the 4 KiB user leaf at `va`, or `PhysAddr::null()`
/// if no 4 KiB leaf is present — a huge leaf reads as null too.
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
            // The result is byte-exact, so the page offset goes back in.
            let off = va.as_u64() & (PAGE_SIZE_4KB - 1);
            PhysAddr::new(p.as_u64() | off)
        }
        _ => PhysAddr::new(0),
    }
}
