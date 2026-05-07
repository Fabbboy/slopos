//! Bounds-checked unaligned `T` read from a byte slice.
//!
//! The kernel's ACPI / MADT / MCFG / HPET parsers walk
//! `#[repr(C, packed)]` structs that live inside firmware-supplied
//! tables. The natural read pattern — `*(bytes.as_ptr() as *const T)`
//! — is unsafe because of (a) potentially-misaligned base, and
//! (b) potentially-truncated trailing bytes. This helper resolves both
//! with one `core::ptr::read_unaligned` behind an explicit bounds
//! check.

use crate::mm::Pod;

/// Read a `T: Pod` from `bytes` at the given offset, copying bytes
/// into the result by value. The base pointer is not required to be
/// aligned to `T`'s alignment; reads are issued as `read_unaligned`.
///
/// Returns `None` if `offset + size_of::<T>() > bytes.len()`.
#[inline]
pub fn read_packed<T: Pod>(bytes: &[u8], offset: usize) -> Option<T> {
    let needed = core::mem::size_of::<T>();
    if offset.checked_add(needed)? > bytes.len() {
        return None;
    }
    let p = bytes.as_ptr().wrapping_add(offset) as *const T;
    // SAFETY: bounds-checked above; `T: Pod` guarantees any byte
    // pattern is a valid value of `T`. `read_unaligned` lifts the
    // alignment requirement.
    Some(unsafe { core::ptr::read_unaligned(p) })
}
