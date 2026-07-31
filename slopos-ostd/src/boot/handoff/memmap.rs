//! Memory-map entry array handoff.
//!
//! The bootloader publishes an array of `MemmapEntry` (base, length,
//! type) describing physical-memory regions. The OSTD primitive
//! consolidates the `slice::from_raw_parts` call so consumers receive
//! a typed `&'static [MemmapEntry]`.

use core::ptr::NonNull;

/// A single entry from the bootloader-published memory map.
///
/// Field layout matches Limine's `memmap::Entry` shape (base / length
/// / type) so the kernel-side adapter in `boot/limine_protocol.rs`
/// can copy directly into this struct.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemmapEntry {
    /// Physical base address of the region.
    pub base: u64,
    /// Length of the region in bytes.
    pub length: u64,
    /// Bootloader-defined region type code (1 = usable, etc.).
    pub typ: u64,
}

/// Borrow the bootloader-published memmap entry array as a typed
/// `&'static [MemmapEntry]`.
///
/// # Why this is safe to call
///
/// The bootloader keeps the memmap response struct alive for the
/// kernel's lifetime, so a `'static` borrow is sound. Callers are
/// responsible for ensuring `entries` actually points at `count`
/// well-formed `MemmapEntry` values — the kernel-side adapter in
/// `boot/limine_protocol.rs` is the only caller and threads through
/// Limine's authoritative count.
pub fn memmap_handoff(entries: NonNull<MemmapEntry>, count: usize) -> &'static [MemmapEntry] {
    // SAFETY: bootloader-published, kernel-lifetime backing; layout
    // matches `#[repr(C)]` MemmapEntry; `count` is bounded by Limine's
    // entry-count field.
    unsafe { core::slice::from_raw_parts(entries.as_ptr(), count) }
}
