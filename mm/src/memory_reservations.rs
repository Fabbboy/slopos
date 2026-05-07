use core::ffi::{c_char, c_int};

use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::util::cstr::cstr_from_kernel_ptr;
use slopos_utils::{align_down_u64, align_up_u64, klog_info};

use crate::memory_layout_defs::KERNEL_VIRTUAL_BASE;
use crate::paging_defs::PAGE_SIZE_4KB;

const MM_REGION_STATIC_CAP: usize = 4096;

pub const MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS: u32 = 1 << 0;
pub const MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT: u32 = 1 << 1;
pub const MM_RESERVATION_FLAG_MMIO: u32 = 1 << 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmReservationType {
    AllocatorMetadata = 0,
    Framebuffer = 1,
    AcpiReclaimable = 2,
    AcpiNvs = 3,
    Apic = 4,
    FirmwareOther = 5,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmRegionKind {
    Usable = 0,
    Reserved = 1,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MmRegion {
    pub phys_base: u64,
    pub length: u64,
    pub kind: MmRegionKind,
    pub type_: MmReservationType,
    pub flags: u32,
    pub label: [u8; 32],
}

impl MmRegion {
    pub const fn zeroed() -> Self {
        Self {
            phys_base: 0,
            length: 0,
            kind: MmRegionKind::Reserved,
            type_: MmReservationType::AllocatorMetadata,
            flags: 0,
            label: [0; 32],
        }
    }
}

struct RegionStoreInner {
    regions: [MmRegion; MM_REGION_STATIC_CAP],
    count: u32,
    overflows: u32,
}

impl RegionStoreInner {
    const fn capacity(&self) -> u32 {
        MM_REGION_STATIC_CAP as u32
    }
}

static REGION_STORE: SpinLock<RegionStoreInner> = SpinLock::new(
    RegionStoreInner {
        regions: [MmRegion::zeroed(); MM_REGION_STATIC_CAP],
        count: 0,
        overflows: 0,
    },
    LOCK_LEVEL_RESOURCE,
);

fn with_store<R>(f: impl FnOnce(&mut RegionStoreInner) -> R) -> R {
    let mut guard = REGION_STORE.lock();
    f(&mut guard)
}

fn clear_store_inner(store: &mut RegionStoreInner) {
    for slot in store.regions.iter_mut() {
        *slot = MmRegion::zeroed();
    }
    store.count = 0;
    store.overflows = 0;
}

fn copy_label(dest: &mut [u8; 32], src: *const c_char) {
    let Some(bytes) = cstr_from_kernel_ptr(src) else {
        dest[0] = 0;
        return;
    };
    let take = bytes.len().min(31);
    dest[..take].copy_from_slice(&bytes[..take]);
    dest[take] = 0;
}

fn insert_slot(store: &mut RegionStoreInner, index: u32) -> Result<(), ()> {
    if store.count >= store.capacity() {
        store.overflows = store.overflows.saturating_add(1);
        return Err(());
    }

    let cap = store.count as usize;
    let idx = (index as usize).min(cap);
    if cap > 0 && idx < cap {
        // shift `[idx, count)` one slot to the right.
        store.regions.copy_within(idx..cap, idx + 1);
    }
    store.count += 1;
    store.regions[idx] = MmRegion::zeroed();
    Ok(())
}

fn regions_equivalent(a: &MmRegion, b: &MmRegion) -> bool {
    if a.kind != b.kind {
        return false;
    }
    if matches!(a.kind, MmRegionKind::Usable) {
        a.flags == b.flags && a.label[0] == b.label[0]
    } else {
        a.type_ == b.type_ && a.flags == b.flags && a.label == b.label
    }
}

fn try_merge_with_neighbors(store: &mut RegionStoreInner, index: u32) {
    let count = store.count;
    if count == 0 || index >= count {
        return;
    }
    let i = index as usize;

    // Merge with previous.
    if index > 0 {
        let prev_end = store.regions[i - 1].phys_base + store.regions[i - 1].length;
        let merge = prev_end == store.regions[i].phys_base && {
            let prev = store.regions[i - 1];
            let curr = store.regions[i];
            regions_equivalent(&prev, &curr)
        };
        if merge {
            store.regions[i - 1].length = store.regions[i - 1]
                .length
                .wrapping_add(store.regions[i].length);
            let cap = store.count as usize;
            if i + 1 < cap {
                store.regions.copy_within(i + 1..cap, i);
            }
            store.count -= 1;
        }
    }

    // Merge with next (re-read count after possible previous merge).
    let count = store.count;
    if (index + 1) < count {
        let i = index as usize;
        let curr_end = store.regions[i].phys_base + store.regions[i].length;
        let merge = curr_end == store.regions[i + 1].phys_base && {
            let curr = store.regions[i];
            let next = store.regions[i + 1];
            regions_equivalent(&curr, &next)
        };
        if merge {
            store.regions[i].length = store.regions[i]
                .length
                .wrapping_add(store.regions[i + 1].length);
            let cap = store.count as usize;
            if i + 2 < cap {
                store.regions.copy_within(i + 2..cap, i + 1);
            }
            store.count -= 1;
        }
    }
}

fn find_region_index(store: &RegionStoreInner, phys_base: u64) -> u32 {
    let mut idx = 0u32;
    while idx < store.count {
        let region = &store.regions[idx as usize];
        if region.phys_base + region.length > phys_base {
            break;
        }
        idx += 1;
    }
    idx
}

fn split_region(store: &mut RegionStoreInner, index: u32, split_base: u64) -> Result<(), ()> {
    if index >= store.count {
        return Err(());
    }
    let i = index as usize;
    let region = store.regions[i];
    let region_end = region.phys_base + region.length;
    if split_base <= region.phys_base || split_base >= region_end {
        return Ok(());
    }

    insert_slot(store, index + 1)?;
    let i = index as usize;
    let mut right = region;
    right.phys_base = split_base;
    right.length = region_end - split_base;
    store.regions[i + 1] = right;
    store.regions[i].length = split_base - store.regions[i].phys_base;
    Ok(())
}

fn overlay_region(
    phys_base: u64,
    length: u64,
    kind: MmRegionKind,
    type_: MmReservationType,
    flags: u32,
    label: *const c_char,
) -> c_int {
    if length == 0 {
        return -1;
    }

    if phys_base >= KERNEL_VIRTUAL_BASE {
        klog_info!("MM: rejecting virtual overlay base 0x{:x}", phys_base);
        return -1;
    }
    if crate::hhdm::is_available() && phys_base >= crate::hhdm::offset() {
        klog_info!("MM: rejecting virtual overlay base 0x{:x}", phys_base);
        return -1;
    }

    let end = phys_base.wrapping_add(length);
    if end <= phys_base {
        return -1;
    }

    let aligned_base = align_down_u64(phys_base, PAGE_SIZE_4KB);
    let aligned_end = align_up_u64(end, PAGE_SIZE_4KB);
    if aligned_end <= aligned_base {
        return -1;
    }

    with_store(|store| {
        let mut cursor = aligned_base;
        while cursor < aligned_end {
            let idx = find_region_index(store, cursor);

            let region_exists = idx < store.count;
            let needs_insert = !region_exists || store.regions[idx as usize].phys_base > cursor;
            if needs_insert {
                if insert_slot(store, idx).is_err() {
                    return -1;
                }
                let i = idx as usize;
                let region = &mut store.regions[i];
                region.phys_base = cursor;
                region.length = aligned_end - cursor;
                region.kind = kind;
                region.type_ = type_;
                region.flags = flags;
                copy_label(&mut region.label, label);
                try_merge_with_neighbors(store, idx);
                break;
            }

            if split_region(store, idx, cursor).is_err() {
                return -1;
            }
            let i = idx as usize;
            let region_end = store.regions[i].phys_base + store.regions[i].length;
            let apply_end = if aligned_end < region_end {
                aligned_end
            } else {
                region_end
            };
            if split_region(store, idx, apply_end).is_err() {
                return -1;
            }

            let i = idx as usize;
            let region = &mut store.regions[i];
            region.kind = kind;
            region.type_ = type_;
            region.flags = flags;
            copy_label(&mut region.label, label);
            try_merge_with_neighbors(store, idx);

            cursor = apply_end;
        }
        0
    })
}

pub fn mm_region_map_reset() {
    with_store(clear_store_inner);
}

pub fn mm_region_add_usable(phys_base: u64, length: u64, label: *const c_char) -> c_int {
    if length == 0 {
        return -1;
    }
    overlay_region(
        phys_base,
        length,
        MmRegionKind::Usable,
        MmReservationType::FirmwareOther,
        0,
        label,
    )
}

pub fn mm_region_reserve(
    phys_base: u64,
    length: u64,
    type_: MmReservationType,
    flags: u32,
    label: *const c_char,
) -> c_int {
    if length == 0 {
        return -1;
    }
    overlay_region(
        phys_base,
        length,
        MmRegionKind::Reserved,
        type_,
        flags,
        label,
    )
}

pub fn mm_region_count() -> u32 {
    with_store(|store| store.count)
}

pub fn mm_region_get(index: u32) -> Option<MmRegion> {
    with_store(|store| {
        if index >= store.count {
            None
        } else {
            Some(store.regions[index as usize])
        }
    })
}

pub fn mm_reservations_count() -> u32 {
    with_store(|store| {
        let mut count = 0;
        for i in 0..store.count as usize {
            let region = &store.regions[i];
            if matches!(region.kind, MmRegionKind::Reserved) && region.length > 0 {
                count += 1;
            }
        }
        count
    })
}

pub fn mm_reservations_capacity() -> u32 {
    with_store(|store| store.capacity())
}

pub fn mm_reservations_overflow_count() -> u32 {
    with_store(|store| store.overflows)
}

pub fn mm_reservations_get(index: u32) -> Option<MmRegion> {
    with_store(|store| {
        let mut seen = 0;
        for i in 0..store.count as usize {
            let region = &store.regions[i];
            if !matches!(region.kind, MmRegionKind::Reserved) || region.length == 0 {
                continue;
            }
            if seen == index {
                return Some(*region);
            }
            seen += 1;
        }
        None
    })
}

pub fn mm_reservations_find(phys_addr: u64) -> Option<MmRegion> {
    with_store(|store| {
        for i in 0..store.count as usize {
            let region = &store.regions[i];
            if !matches!(region.kind, MmRegionKind::Reserved) || region.length == 0 {
                continue;
            }
            let end = region.phys_base + region.length;
            if phys_addr >= region.phys_base && phys_addr < end {
                return Some(*region);
            }
        }
        None
    })
}

pub fn mm_reservations_find_option(phys_addr: u64) -> Option<MmRegion> {
    mm_reservations_find(phys_addr)
}

pub fn mm_reservation_type_name(type_: MmReservationType) -> &'static str {
    match type_ {
        MmReservationType::AllocatorMetadata => "allocator metadata",
        MmReservationType::Framebuffer => "framebuffer",
        MmReservationType::AcpiReclaimable => "acpi reclaim",
        MmReservationType::AcpiNvs => "acpi nvs",
        MmReservationType::Apic => "apic",
        MmReservationType::FirmwareOther => "firmware",
    }
}

pub fn mm_reservations_total_bytes(required_flags: u32) -> u64 {
    with_store(|store| {
        let mut total = 0u64;
        for i in 0..store.count as usize {
            let region = &store.regions[i];
            if !matches!(region.kind, MmRegionKind::Reserved) || region.length == 0 {
                continue;
            }
            if required_flags != 0 && (region.flags & required_flags) != required_flags {
                continue;
            }
            total = total.wrapping_add(region.length);
        }
        total
    })
}

pub fn mm_region_total_bytes(kind: MmRegionKind) -> u64 {
    with_store(|store| {
        let mut total = 0u64;
        for i in 0..store.count as usize {
            let region = &store.regions[i];
            if region.kind == kind {
                total = total.wrapping_add(region.length);
            }
        }
        total
    })
}

pub fn mm_region_highest_usable_frame() -> u64 {
    with_store(|store| {
        let mut highest = 0u64;
        for i in 0..store.count as usize {
            let region = &store.regions[i];
            if !matches!(region.kind, MmRegionKind::Usable) || region.length == 0 {
                continue;
            }
            let end = region.phys_base + region.length - 1;
            let frame = end >> 12;
            if frame > highest {
                highest = frame;
            }
        }
        highest
    })
}

/// Returns the highest frame index seen across **any** memory-map region
/// (Usable, Reserved, KernelAndModules, Bootloader-Reclaimable,
/// AcpiReclaimable, AcpiNvs, BadMemory, …). The OSTD `META_SLOTS` array
/// must cover every paddr the kernel might wrap with `Frame<M>` —
/// including the kernel image, the bootloader-allocated PML4, and any
/// framebuffer / ACPI region that gets mapped via cursor. The "usable"
/// variant above is too narrow for that.
pub fn mm_region_highest_frame_seen() -> u64 {
    with_store(|store| {
        let mut highest = 0u64;
        for i in 0..store.count as usize {
            let region = &store.regions[i];
            if region.length == 0 {
                continue;
            }
            let end = region.phys_base.saturating_add(region.length - 1);
            let frame = end >> 12;
            if frame > highest {
                highest = frame;
            }
        }
        highest
    })
}
