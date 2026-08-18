//! Bounds-checked unaligned `T` read from a byte slice, for the
//! `#[repr(C, packed)]` structs in firmware-supplied ACPI tables: the base may
//! be misaligned and the trailing bytes truncated.

use crate::mm::Pod;

/// Read a `T: Pod` from `bytes` at `offset`; the base need not be aligned to
/// `T`'s alignment.
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
