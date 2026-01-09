use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_lib::{align_down_u64, align_up_u64, klog_info};

use crate::mm_constants::{KERNEL_VIRTUAL_BASE, PAGE_SIZE_4KB};

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
struct RegionStore {
    regions: *mut MmRegion,
    capacity: u32,
    count: u32,
    overflows: u32,
    configured: bool,
}

unsafe impl Send for RegionStore {}
unsafe impl Sync for RegionStore {}

static mut STATIC_REGION_STORE: [MmRegion; MM_REGION_STATIC_CAP] =
    [MmRegion::zeroed(); MM_REGION_STATIC_CAP];
static mut REGION_STORE: RegionStore = RegionStore {
    regions: unsafe { STATIC_REGION_STORE.as_ptr() as *mut MmRegion },
    capacity: MM_REGION_STATIC_CAP as u32,
    count: 0,
    overflows: 0,
    configured: false,
};

fn ensure_storage() -> &'static mut RegionStore {
    unsafe {
        if REGION_STORE.regions.is_null() || REGION_STORE.capacity == 0 {
            panic!("MM: region storage not configured");
        }
        &mut REGION_STORE
    }
}

fn clear_region(region: &mut MmRegion) {
    *region = MmRegion::zeroed();
}

fn clear_store() {
    let store = ensure_storage();
    unsafe {
        for i in 0..store.capacity as usize {
            clear_region(&mut *store.regions.add(i));
        }
    }
    store.count = 0;
    store.overflows = 0;
}

fn copy_label(dest: &mut [u8; 32], src: *const c_char) {
    if src.is_null() {
        dest[0] = 0;
        return;
    }

    let mut i = 0;
    unsafe {
        while i < 31 {
            let ch = *src.add(i) as u8;
            if ch == 0 {
                break;
            }
            dest[i] = ch;
            i += 1;
        }
    }
    dest[i] = 0;
}

fn insert_slot(index: u32) -> Result<(), ()> {
    let store = ensure_storage();
    if store.count >= store.capacity {
        store.overflows = store.overflows.saturating_add(1);
        return Err(());
    }

    let idx = index.min(store.count);
    if store.count > 0 && idx < store.count {
        unsafe {
            let dst = store.regions.add((idx + 1) as usize);
            let src = store.regions.add(idx as usize);
            let move_elems = (store.count - idx) as usize;
            ptr::copy(src, dst, move_elems);
        }
    }
    store.count += 1;
    unsafe {
        clear_region(&mut *store.regions.add(idx as usize));
    }
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

fn try_merge_with_neighbors(index: u32) {
    let store = ensure_storage();
    if store.count == 0 || index >= store.count {
        return;
    }

    // Merge with previous
    if index > 0 {
        let curr = unsafe { &mut *store.regions.add(index as usize) };
        let prev = unsafe { &mut *store.regions.add((index - 1) as usize) };
        let prev_end = prev.phys_base + prev.length;
        if prev_end == curr.phys_base && regions_equivalent(prev, curr) {
            prev.length = prev.length.wrapping_add(curr.length);
            unsafe {
                let src = store.regions.add(index as usize + 1);
                let dst = store.regions.add(index as usize);
                let move_elems = (store.count - index - 1) as usize;
                ptr::copy(src, dst, move_elems);
            }
            store.count -= 1;
        }
    }

    // Merge with next
    if index + 1 < store.count {
        let curr = unsafe { &mut *store.regions.add(index as usize) };
        let next = unsafe { &mut *store.regions.add(index as usize + 1) };
        let curr_end = curr.phys_base + curr.length;
        if curr_end == next.phys_base && regions_equivalent(curr, next) {
            curr.length = curr.length.wrapping_add(next.length);
            unsafe {
                let src = store.regions.add(index as usize + 2);
                let dst = store.regions.add(index as usize + 1);
                let move_elems = (store.count - index - 2) as usize;
                ptr::copy(src, dst, move_elems);
            }
            store.count -= 1;
        }
    }
}

fn find_region_index(phys_base: u64) -> u32 {
    let store = ensure_storage();
    let mut idx = 0;
    while idx < store.count {
        let region = unsafe { &*store.regions.add(idx as usize) };
        if region.phys_base + region.length > phys_base {
            break;
        }
        idx += 1;
    }
    idx
}

fn split_region(index: u32, split_base: u64) -> Result<(), ()> {
    let store = ensure_storage();
    if index >= store.count {
        return Err(());
    }
    let region = unsafe { &mut *store.regions.add(index as usize) };
    let region_end = region.phys_base + region.length;
    if split_base <= region.phys_base || split_base >= region_end {
        return Ok(());
    }

    insert_slot(index + 1)?;
    let right = unsafe { &mut *store.regions.add(index as usize + 1) };
    *right = *region;
    right.phys_base = split_base;
    right.length = region_end - split_base;
    region.length = split_base - region.phys_base;
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

    let mut cursor = aligned_base;
    while cursor < aligned_end {
        let idx = find_region_index(cursor);
        let store = ensure_storage();

        let region_exists = idx < store.count;
        if !region_exists || unsafe { (*store.regions.add(idx as usize)).phys_base > cursor } {
            if insert_slot(idx).is_err() {
                return -1;
            }
            let region = unsafe { &mut *store.regions.add(idx as usize) };
            region.phys_base = cursor;
            region.length = aligned_end - cursor;
            region.kind = kind;
            region.type_ = type_;
            region.flags = flags;
            copy_label(&mut region.label, label);
            try_merge_with_neighbors(idx);
            break;
        }

        if split_region(idx, cursor).is_err() {
            return -1;
        }
        let region = unsafe { &mut *store.regions.add(idx as usize) };
        let region_end = region.phys_base + region.length;

        let apply_end = if aligned_end < region_end {
            aligned_end
        } else {
            region_end
        };
        if split_region(idx, apply_end).is_err() {
            return -1;
        }

        let region = unsafe { &mut *store.regions.add(idx as usize) };
        region.kind = kind;
        region.type_ = type_;
        region.flags = flags;
        copy_label(&mut region.label, label);
        try_merge_with_neighbors(idx);

        cursor = apply_end;
    }

    0
}
pub fn mm_region_map_configure(buffer: *mut MmRegion, capacity: u32) {
    if buffer.is_null() || capacity == 0 {
        panic!("MM: invalid region storage configuration");
    }
    unsafe {
        REGION_STORE.regions = buffer;
        REGION_STORE.capacity = capacity;
        REGION_STORE.configured = true;
    }
    clear_store();
}
pub fn mm_region_map_reset() {
    unsafe {
        if !REGION_STORE.configured {
            REGION_STORE.regions = STATIC_REGION_STORE.as_mut_ptr();
            REGION_STORE.capacity = MM_REGION_STATIC_CAP as u32;
            REGION_STORE.configured = true;
        }
    }
    clear_store();
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
    ensure_storage().count
}
pub fn mm_region_get(index: u32) -> *const MmRegion {
    let store = ensure_storage();
    if index >= store.count {
        return ptr::null();
    }
    unsafe { store.regions.add(index as usize) }
}
pub fn mm_reservations_count() -> u32 {
    let store = ensure_storage();
    let mut count = 0;
    for i in 0..store.count {
        let region = unsafe { &*store.regions.add(i as usize) };
        if matches!(region.kind, MmRegionKind::Reserved) && region.length > 0 {
            count += 1;
        }
    }
    count
}
pub fn mm_reservations_capacity() -> u32 {
    ensure_storage().capacity
}
pub fn mm_reservations_overflow_count() -> u32 {
    ensure_storage().overflows
}
pub fn mm_reservations_get(index: u32) -> *const MmRegion {
    let store = ensure_storage();
    let mut seen = 0;
    for i in 0..store.count {
        let region = unsafe { &*store.regions.add(i as usize) };
        if !matches!(region.kind, MmRegionKind::Reserved) || region.length == 0 {
            continue;
        }
        if seen == index {
            return region as *const MmRegion;
        }
        seen += 1;
    }
    ptr::null()
}
pub fn mm_reservations_find(phys_addr: u64) -> *const MmRegion {
    let store = ensure_storage();
    for i in 0..store.count {
        let region = unsafe { &*store.regions.add(i as usize) };
        if !matches!(region.kind, MmRegionKind::Reserved) || region.length == 0 {
            continue;
        }
        let end = region.phys_base + region.length;
        if phys_addr >= region.phys_base && phys_addr < end {
            return region as *const MmRegion;
        }
    }
    ptr::null()
}

pub fn mm_reservations_find_option(phys_addr: u64) -> Option<&'static MmRegion> {
    let ptr = mm_reservations_find(phys_addr);
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}
pub fn mm_reservation_type_name(type_: MmReservationType) -> *const c_char {
    match type_ {
        MmReservationType::AllocatorMetadata => b"allocator metadata\0".as_ptr() as *const c_char,
        MmReservationType::Framebuffer => b"framebuffer\0".as_ptr() as *const c_char,
        MmReservationType::AcpiReclaimable => b"acpi reclaim\0".as_ptr() as *const c_char,
        MmReservationType::AcpiNvs => b"acpi nvs\0".as_ptr() as *const c_char,
        MmReservationType::Apic => b"apic\0".as_ptr() as *const c_char,
        MmReservationType::FirmwareOther => b"firmware\0".as_ptr() as *const c_char,
    }
}
pub fn mm_reservations_total_bytes(required_flags: u32) -> u64 {
    let store = ensure_storage();
    let mut total = 0u64;
    for i in 0..store.count {
        let region = unsafe { &*store.regions.add(i as usize) };
        if !matches!(region.kind, MmRegionKind::Reserved) || region.length == 0 {
            continue;
        }
        if required_flags != 0 && (region.flags & required_flags) != required_flags {
            continue;
        }
        total = total.wrapping_add(region.length);
    }
    total
}
pub fn mm_region_total_bytes(kind: MmRegionKind) -> u64 {
    let store = ensure_storage();
    let mut total = 0u64;
    for i in 0..store.count {
        let region = unsafe { &*store.regions.add(i as usize) };
        if region.kind == kind {
            total = total.wrapping_add(region.length);
        }
    }
    total
}
pub fn mm_region_highest_usable_frame() -> u64 {
    let store = ensure_storage();
    let mut highest = 0u64;
    for i in 0..store.count {
        let region = unsafe { &*store.regions.add(i as usize) };
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
}
