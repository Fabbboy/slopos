//! Safe `&[T] ↔ &[u8]` conversions for `T: Pod`.
//!
//! Centralises the `from_raw_parts` casts that copying arrays of POD records
//! to and from user buffers needs, behind a typed safe API.

use core::mem::MaybeUninit;

use crate::mm::Pod;

/// Reinterpret a `&[T]` as a `&[u8]`; length scales by `size_of::<T>()`.
#[inline]
pub fn pod_slice_as_bytes<T: Pod>(slice: &[T]) -> &[u8] {
    let len = slice.len() * core::mem::size_of::<T>();
    // SAFETY: `T: Pod` allows arbitrary byte access; `slice.as_ptr()`
    // is non-null and aligned for `T`. We weaken the alignment claim
    // by the cast to `*const u8`, which is always 1-aligned.
    unsafe { core::slice::from_raw_parts(slice.as_ptr() as *const u8, len) }
}

/// Mutable sibling of [`pod_slice_as_bytes`].
#[inline]
pub fn pod_slice_as_bytes_mut<T: Pod>(slice: &mut [T]) -> &mut [u8] {
    let len = slice.len() * core::mem::size_of::<T>();
    // SAFETY: see `pod_slice_as_bytes`. The mutable borrow uniquely
    // covers `slice` for the returned lifetime.
    unsafe { core::slice::from_raw_parts_mut(slice.as_mut_ptr() as *mut u8, len) }
}

/// Reinterpret a single `&T: Pod` as `&[u8]` of length `size_of::<T>()`.
#[inline]
pub fn pod_as_bytes<T: Pod>(value: &T) -> &[u8] {
    pod_slice_as_bytes(core::slice::from_ref(value))
}

/// Mutable byte view of a `MaybeUninit<T>`. Sound for **any** `T`, since
/// `MaybeUninit` permits arbitrary byte writes; the caller must only invoke
/// `assume_init` once the bytes represent a valid `T`.
#[inline]
pub fn maybe_uninit_as_bytes_mut<T>(val: &mut MaybeUninit<T>) -> &mut [u8] {
    // SAFETY: a `MaybeUninit<T>` occupies `size_of::<T>()` bytes of
    // storage and is freely writable through a byte pointer; we hold
    // a `&mut` reference so no aliasing borrow exists.
    unsafe {
        core::slice::from_raw_parts_mut(val.as_mut_ptr() as *mut u8, core::mem::size_of::<T>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_as_bytes_round_trip_u32() {
        let v: u32 = 0xdead_beef;
        let bytes = pod_as_bytes(&v);
        assert_eq!(bytes.len(), 4);
        assert_eq!(bytes, &v.to_ne_bytes()[..]);
    }

    #[test]
    fn pod_as_bytes_matches_pod_slice_as_bytes_for_one() {
        let arr: [u8; 5] = [1, 2, 3, 4, 5];
        assert_eq!(
            pod_as_bytes(&arr),
            pod_slice_as_bytes(core::slice::from_ref(&arr))
        );
    }

    #[test]
    fn maybe_uninit_as_bytes_mut_writes_and_assumes_init() {
        let mut val: MaybeUninit<u32> = MaybeUninit::uninit();
        let bytes = maybe_uninit_as_bytes_mut(&mut val);
        assert_eq!(bytes.len(), 4);
        bytes.copy_from_slice(&0x1234_5678u32.to_ne_bytes());
        // SAFETY: bytes above were written for the entire value.
        let v = unsafe { val.assume_init() };
        assert_eq!(v, 0x1234_5678);
    }

    #[test]
    fn maybe_uninit_as_bytes_mut_sized_correctly_for_array() {
        let mut val: MaybeUninit<[u8; 7]> = MaybeUninit::uninit();
        let bytes = maybe_uninit_as_bytes_mut(&mut val);
        assert_eq!(bytes.len(), 7);
    }
}
