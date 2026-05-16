//! Safe-Rust slab poisoning.
//!
//! Bytes returned to the free pool are filled with [`POISON_FREED`]
//! so a use-after-free reader sees a recognisable pattern instead of
//! the previous owner's data. The fill is performed through OSTD's
//! safe `ptr_buf::borrow_buf_mut` helper — the residual `unsafe` for
//! the raw-pointer-to-slice conversion lives inside OSTD.
//!
//! `USegment::write_bytes` is not used here because slab pages are
//! kernel-owned memory (the `AnyUFrameMeta` safety contract in
//! `slopos-ostd/src/mm/uframe.rs` excludes sensitive kernel metas
//! from the untyped-byte-copy surface). `ptr_buf::borrow_buf_mut` is
//! OSTD's canonical safe-byte-write primitive for this case.

/// Byte pattern written into freed slab objects. Chosen so a
/// use-after-free dereference fault on a typical pointer-shaped read
/// is conspicuous (0x6B repeated → 0x6B6B6B6B6B6B6B6B, a non-canonical
/// pointer that the CPU traps on as a #GP).
pub(crate) const POISON_FREED: u8 = 0x6B;

/// Fill `body` with `pattern`. Trivial — exists so the call site reads
/// as "poison this object body" rather than `body.fill(byte)` and to
/// give a single anchor for future poison-strategy changes (e.g.,
/// per-object incrementing counters, dword guards, etc.).
#[inline]
pub(crate) fn poison_object_body(body: &mut [u8], pattern: u8) {
    body.fill(pattern);
}
