//! Memory-map entry array handoff.

use core::ptr::NonNull;

/// A single entry from the bootloader-published memory map.
///
/// Field order matches Limine's `memmap::Entry` (base / length / type).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    /// Bootloader-defined region type code (1 = usable, etc.).
    pub typ: u64,
}

/// Borrow the bootloader-published memmap entry array.
///
/// The bootloader keeps the response struct alive for the kernel's lifetime,
/// so the `'static` borrow is sound; the caller must ensure `entries` points
/// at `count` well-formed `MemmapEntry` values.
pub fn memmap_handoff(entries: NonNull<MemmapEntry>, count: usize) -> &'static [MemmapEntry] {
    // SAFETY: bootloader-published, kernel-lifetime backing; layout
    // matches `#[repr(C)]` MemmapEntry; `count` is bounded by Limine's
    // entry-count field.
    unsafe { core::slice::from_raw_parts(entries.as_ptr(), count) }
}
