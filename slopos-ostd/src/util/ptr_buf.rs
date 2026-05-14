//! Raw-pointer-to-slice borrow primitives.
//!
//! Kernel-half code receives raw `*const T` / `*mut T` / `NonNull<T>`
//! pointers from FFI boundaries (bootloader handoff, C-ABI syscall
//! adapters, linker section anchors, NonNull-bearing kernel allocations)
//! and needs to expose them as `&[T]` / `&mut [T]` for the surrounding
//! safe Rust code. Each helper folds one
//! `unsafe { core::slice::from_raw_parts(ptr, len) }` call interior to
//! OSTD; the caller receives a safe slice.
//!
//! # Safety contract on every helper
//!
//! Each function is **safe to call**. The interior `unsafe` is sound
//! whenever the caller ensures:
//!
//! - `ptr` is valid for reads (and writes, for the `_mut` variants) of
//!   `len` consecutive `T` values,
//! - `ptr` is aligned for `T`,
//! - the chosen lifetime `'a` does not outlive the underlying storage,
//! - while the returned `&[T]` / `&mut [T]` is live, no other code
//!   creates an aliasing mutable / shared borrow of the same range,
//! - reads through the returned slice see well-initialized `T` values
//!   (writes through `_mut` variants do not require pre-initialization).
//!
//! Doc-comments on each function add site-specific guidance where
//! relevant. These preconditions mirror the standard `from_raw_parts`
//! / `from_raw_parts_mut` contracts; the only thing that moves is the
//! `unsafe` keyword's location.

use core::ptr::NonNull;

/// Borrow `len` elements of `T` starting at `ptr` as a `&[T]`.
///
/// Companion of `core::slice::from_raw_parts`. See module-level
/// safety contract.
#[inline]
pub fn borrow_buf<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    // SAFETY: caller upholds the module-level contract; this is the
    // single `from_raw_parts` call site for all kernel-half consumers
    // of the `*const T + len` pattern.
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

/// Mutable sibling of [`borrow_buf`].
#[inline]
pub fn borrow_buf_mut<'a, T>(ptr: *mut T, len: usize) -> &'a mut [T] {
    // SAFETY: caller upholds the module-level contract, including
    // exclusive ownership of `[ptr, ptr+len)` for the returned lifetime.
    unsafe { core::slice::from_raw_parts_mut(ptr, len) }
}

/// Borrow `len` elements of `T` starting at `ptr` (a `NonNull`) as
/// a `&[T]`. Equivalent to [`borrow_buf`] with the null-pointer
/// branch elided.
#[inline]
pub fn borrow_nonnull<'a, T>(ptr: NonNull<T>, len: usize) -> &'a [T] {
    // SAFETY: `NonNull::as_ptr` returns a non-null pointer; remaining
    // preconditions are the module-level contract.
    unsafe { core::slice::from_raw_parts(ptr.as_ptr(), len) }
}

/// Mutable sibling of [`borrow_nonnull`].
#[inline]
pub fn borrow_nonnull_mut<'a, T>(ptr: NonNull<T>, len: usize) -> &'a mut [T] {
    // SAFETY: `NonNull::as_ptr` returns a non-null pointer; remaining
    // preconditions are the module-level contract.
    unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), len) }
}

/// Borrow `len` elements of `T` starting at `(base as *mut u8) + byte_offset`
/// as `&mut [T]`. Folds the kernel-heap pattern of "advance a NonNull
/// by a byte offset, then take a typed slice of `len` Ts" into one call.
///
/// Used in `mm/src/kernel_heap.rs` to view a slab object's body or a
/// large-alloc body that lives at a fixed byte offset past a header
/// pointer.
#[inline]
pub fn borrow_at_mut<'a, T>(base: NonNull<u8>, byte_offset: usize, len: usize) -> &'a mut [T] {
    // SAFETY: caller upholds the module-level contract; `base` is
    // NonNull and the byte arithmetic stays inside the caller-owned
    // allocation.
    unsafe {
        let typed_ptr = base.as_ptr().add(byte_offset) as *mut T;
        core::slice::from_raw_parts_mut(typed_ptr, len)
    }
}

/// Slice between two `*const T` anchors. Returns `&start[..(stop -
/// start)]`. If `stop <= start`, returns an empty slice rather than
/// panicking.
///
/// Used for linker-section iteration where the linker exports
/// `__start_<section>` and `__stop_<section>` symbols flanking a
/// contiguous array. Absorbs both the `*const T` pointer arithmetic
/// (`stop.offset_from(start)`) and the slice construction.
#[inline]
pub fn section_slice<'a, T>(start: *const T, stop: *const T) -> &'a [T] {
    // SAFETY: caller upholds the module-level contract: `start` and
    // `stop` flank a single contiguous array of `T` values (the linker
    // section). `offset_from` requires both pointers to land in the
    // same allocated object, which the linker guarantees for the
    // `__start_*` / `__stop_*` symbol pair.
    let len = unsafe { stop.offset_from(start) };
    let len = if len < 0 { 0 } else { len as usize };
    // SAFETY: module-level contract.
    unsafe { core::slice::from_raw_parts(start, len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrow_buf_round_trip() {
        let src = [10u32, 20, 30, 40];
        let view = borrow_buf::<u32>(src.as_ptr(), src.len());
        assert_eq!(view, &src[..]);
    }

    #[test]
    fn borrow_buf_mut_writes_visible_through_source() {
        let mut src = [0u32; 4];
        let ptr = src.as_mut_ptr();
        {
            let view: &mut [u32] = borrow_buf_mut(ptr, 4);
            view[0] = 0xaa;
            view[3] = 0xbb;
        }
        assert_eq!(src, [0xaa, 0, 0, 0xbb]);
    }

    #[test]
    fn borrow_nonnull_round_trip() {
        let src = [1u8, 2, 3];
        let ptr = NonNull::new(src.as_ptr() as *mut u8).unwrap();
        let view = borrow_nonnull::<u8>(ptr, src.len());
        assert_eq!(view, &src[..]);
    }

    #[test]
    fn borrow_nonnull_mut_writes_visible_through_source() {
        let mut src = [0u8; 3];
        let ptr = NonNull::new(src.as_mut_ptr()).unwrap();
        {
            let view: &mut [u8] = borrow_nonnull_mut(ptr, 3);
            view.fill(0x42);
        }
        assert_eq!(src, [0x42; 3]);
    }

    #[test]
    fn borrow_at_mut_advances_by_byte_offset() {
        let mut buf = [0u8; 16];
        let base = NonNull::new(buf.as_mut_ptr()).unwrap();
        {
            let view: &mut [u32] = borrow_at_mut::<u32>(base, 4, 2);
            view[0] = 0xdeadbeefu32;
            view[1] = 0x1234_5678u32;
        }
        // First 4 bytes untouched, then u32 LE/BE pattern.
        assert_eq!(&buf[0..4], &[0, 0, 0, 0]);
        assert_eq!(&buf[4..8], &0xdeadbeefu32.to_ne_bytes());
        assert_eq!(&buf[8..12], &0x1234_5678u32.to_ne_bytes());
    }

    #[test]
    fn section_slice_covers_between_anchors() {
        let buf = [11u32, 22, 33, 44, 55];
        let start = buf.as_ptr();
        // SAFETY: in-bounds offset (`add(3)` lands on element 3 of 5).
        let stop = unsafe { start.add(3) };
        let view = section_slice::<u32>(start, stop);
        assert_eq!(view, &[11, 22, 33]);
    }

    #[test]
    fn section_slice_empty_when_stop_before_start() {
        let buf = [11u32, 22];
        let start = buf.as_ptr();
        // SAFETY: in-bounds.
        let later = unsafe { start.add(1) };
        let view = section_slice::<u32>(later, start);
        assert!(view.is_empty());
    }

    #[test]
    fn section_slice_empty_when_stop_equals_start() {
        let buf = [11u32];
        let p = buf.as_ptr();
        assert!(section_slice::<u32>(p, p).is_empty());
    }
}
