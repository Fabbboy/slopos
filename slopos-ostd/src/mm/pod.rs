//! Plain-old-data marker trait for byte-copy-safe types.
//!
//! `Pod` is the gating trait for `UFrame::read_pod` / `write_pod`
//! and the byte-copy MMIO / user-copy primitives that build on it.
//!
//! # Safety
//!
//! Implementing `Pod` asserts that `Self`:
//!   - is `Copy`,
//!   - has a fixed `#[repr(C)]` (or `#[repr(transparent)]` over a
//!     `Pod`),
//!   - has no invalid bit patterns: every byte sequence of length
//!     `size_of::<Self>()` is a valid representation of `Self`,
//!   - has no padding that, if observed by a third party, would
//!     reveal sensitive memory contents.
//!
//! These rules exclude `bool` (only `0x00` and `0x01` are valid),
//! `char` (must be a valid Unicode scalar value), `f32`/`f64` (NaN
//! bit-pattern equivalence concerns; can be added under a
//! deliberately-doc'd impl when a use case forces it), references
//! (`&T`, `&mut T`), and raw pointers (`*const T`, `*mut T`, fat
//! pointers, function pointers).
//!
//! Use `#[derive(Pod)]` (re-exported as `slopos_ostd::Pod` from
//! `slopos-ostd-derive`) for user-defined POD structs; the derive
//! enforces `#[repr(C)]`/`transparent`, rejects `#[repr(packed)]`,
//! and adds a `T: Pod` bound on every field.

/// See module-level docs for safety requirements.
pub unsafe trait Pod: Copy + 'static {}

macro_rules! impl_pod_prims {
    ($($t:ty),* $(,)?) => {
        $(
            // SAFETY: integer primitive — every bit pattern is a valid
            // value and the layout is `#[repr(C)]`-compatible.
            unsafe impl Pod for $t {}
        )*
    };
}
impl_pod_prims!(
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    ()
);

// SAFETY: a contiguous run of `T`, so if `T` is Pod every byte pattern is
// valid for the array.
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}
