//! Safe `&[T] ↔ &[u8]` conversions for `T: Pod`.
//!
//! Kernel-half code that copies arrays of POD records to/from user
//! buffers historically used `unsafe { core::slice::from_raw_parts(p
//! as *const u8, n * size_of::<T>()) }`. This module centralises the
//! unsafe behind a typed safe API.

use crate::mm::Pod;

/// Reinterpret a `&[T]` as a `&[u8]`. Length scales by
/// `size_of::<T>()`. Always succeeds — `T: Pod` guarantees any byte
/// pattern is valid for both directions.
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
