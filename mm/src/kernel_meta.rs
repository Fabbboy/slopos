//! Per-frame metadata tag for untyped kernel pages and the boot-time
//! installer for OSTD's `META_SLOTS` array.
//!
//! `KernelMeta` is the default `AnyFrameMeta` for any kernel-owned
//! page that doesn't need a richer type (page tables, anonymous
//! frames, DMA buffers all carry their own meta types). It lives in
//! `slopos-ostd` already; this module re-exports it so kernel code can
//! reach it under the `slopos_mm::kernel_meta::KernelMeta` path that
//! `slopos_mm::page_alloc::KernelFrame` references.
//!
//! [`install_meta_slots`] sizes the array to one slot per usable
//! physical frame, allocates the backing pages from the buddy
//! allocator, and registers the slice with OSTD via
//! [`slopos_ostd::mm::frame::init_meta_slots`].

use core::mem::size_of;

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::frame::{MAX_META_ALIGN, MAX_META_SIZE, MetaSlot, init_meta_slots};

use crate::hhdm::PhysAddrHhdm;
use crate::memory_reservations::mm_region_highest_frame_seen;
use crate::page_alloc::alloc_page_frames;
use crate::paging_defs::PAGE_SIZE_4KB;

pub use slopos_ostd::mm::frame::KernelMeta;

const _: () = assert!(
    size_of::<KernelMeta>() <= MAX_META_SIZE,
    "KernelMeta exceeds MAX_META_SIZE"
);
const _: () = assert!(
    core::mem::align_of::<KernelMeta>() <= MAX_META_ALIGN,
    "KernelMeta exceeds MAX_META_ALIGN"
);

/// Allocate one [`MetaSlot`] per usable physical 4 KiB frame and
/// install the slice via [`init_meta_slots`]. Must run after the
/// buddy allocator + HHDM are live (Memory phase priority ≥ 10) and
/// before any `Frame<M>` is constructed.
///
/// Returns the number of slots installed, or `0` if there is nothing
/// to install (no usable memory or the buddy returned NULL).
pub fn install_meta_slots() -> usize {
    // Cover every frame in the memory map — `Usable` is too narrow
    // (the kernel PML4 and other bootloader-allocated frames live
    // in `KernelAndModules` / `BootloaderReclaimable` / `Reserved`
    // regions). Any of those can become a `Frame<PageTableMeta>`
    // ref-counted handle later (e.g. `KERNEL_VM_SPACE::wrap_existing`
    // wraps the kernel master PML4 — its slot must exist).
    let highest_frame = mm_region_highest_frame_seen();
    if highest_frame == 0 {
        return 0;
    }
    let n_slots = (highest_frame as usize).saturating_add(1);
    let bytes = match n_slots.checked_mul(size_of::<MetaSlot>()) {
        Some(b) => b,
        None => return 0,
    };
    let page_size = PAGE_SIZE_4KB as usize;
    let pages = bytes.div_ceil(page_size);
    let count = match u32::try_from(pages) {
        Ok(c) if c > 0 => c,
        _ => return 0,
    };

    let phys = alloc_page_frames(count, 0);
    if phys.is_null() {
        panic!(
            "install_meta_slots: alloc_page_frames({} pages, ZERO) returned NULL",
            count
        );
    }

    let virt = PhysAddr::new(phys.as_u64()).to_virt();
    let slots = virt.as_u64() as *mut MetaSlot;

    // SAFETY: `alloc_page_frames(.., ZERO)` returned `pages * 4 KiB` of
    // zero-initialised, page-aligned, exclusively-owned physical memory;
    // `to_virt` translated through the kernel HHDM into a kernel-mode
    // virtual address that is unique to us, so the `MetaSlot` slice
    // covering `[slots, slots + n_slots)` is valid for the kernel's
    // lifetime and does not alias.
    unsafe {
        init_meta_slots(slots, n_slots);
    }
    n_slots
}
