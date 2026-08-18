//! Safe-Rust slab poisoning: bytes returned to the free pool are filled
//! with [`POISON_FREED`] so a use-after-free reader sees a recognisable
//! pattern instead of the previous owner's data.
//!
//! The fill goes through OSTD's scoped `ptr_buf::with_*` rather than
//! `USegment::write_bytes`, whose `AnyUFrameMeta` contract excludes
//! kernel-owned metas like slab pages.

/// Byte pattern written into freed slab objects: 0x6B repeats into a
/// non-canonical pointer, so a use-after-free deref traps as #GP.
pub(crate) const POISON_FREED: u8 = 0x6B;

#[inline]
pub(crate) fn poison_object_body(body: &mut [u8], pattern: u8) {
    body.fill(pattern);
}
