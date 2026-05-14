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

/// Write `value` through `out` if non-null. Folds the kernel-half C-ABI
/// idiom of "caller passes an optional `*mut T` output slot; write if
/// present, else discard" into one helper so consumer crates need no
/// raw-pointer write.
///
/// # Safety contract on the caller
///
/// When `out.is_null()` returns `false`, the caller must ensure `out`
/// points at a valid, properly aligned, writable `T` and is not aliased
/// for the duration of the write. The interior `unsafe` lives here.
#[inline]
pub fn nullable_write<T>(out: *mut T, value: T) {
    if out.is_null() {
        return;
    }
    // SAFETY: caller upholds the per-helper contract above.
    unsafe { core::ptr::write(out, value) };
}

/// Initialise a freshly allocated slot at `dst` with `value`. Mirrors
/// `core::ptr::write(dst, value)` but moves the `unsafe` into OSTD so
/// kernel-half allocator code stays in safe Rust.
///
/// # Safety contract on the caller
///
/// `dst` must be non-null, aligned for `T`, exclusively owned (typically
/// because it was just produced by an allocator like `kmalloc` or
/// `KBox::leak_unsized`), and large enough for one `T`. The old contents
/// (if any) are overwritten without being dropped, so the slot must
/// either be uninitialised or hold a `T` whose drop is intentionally
/// skipped.
#[inline]
pub fn init_slot<T>(dst: *mut T, value: T) {
    debug_assert!(!dst.is_null(), "init_slot: dst must be non-null");
    // SAFETY: caller upholds the contract above.
    unsafe { core::ptr::write(dst, value) };
}

/// Read a possibly-unaligned `T: Copy` at byte `offset` of `payload`.
/// Returns `None` if `offset + size_of::<T>()` exceeds `payload.len()`.
/// Folds the "slice + byte-offset + `read_unaligned`" pattern (the ELF
/// loader / relocation walker uses it heavily) into one safe helper.
#[inline]
pub fn read_pod_at<T: Copy>(payload: &[u8], offset: usize) -> Option<T> {
    let needed = core::mem::size_of::<T>();
    if offset.checked_add(needed)? > payload.len() {
        return None;
    }
    // SAFETY: bounds-checked above; `T: Copy` permits any byte pattern;
    // `read_unaligned` lifts the alignment requirement; the slice's
    // pointer is valid for reads of `needed` bytes from `offset`.
    let p = unsafe { payload.as_ptr().add(offset) } as *const T;
    Some(unsafe { core::ptr::read_unaligned(p) })
}

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

/// Borrow a single `T` value at `ptr` as `&T`. Companion of
/// `&*ptr` — the interior `unsafe` lives here.
///
/// # Safety contract on the caller
///
/// `ptr` must be non-null, aligned for `T`, point at an initialised
/// `T`, and not be aliased by any concurrent mutable borrow for the
/// returned lifetime.
#[inline]
pub fn borrow_ref<'a, T>(ptr: *const T) -> &'a T {
    debug_assert!(!ptr.is_null(), "borrow_ref: ptr must be non-null");
    // SAFETY: caller upholds the contract above.
    unsafe { &*ptr }
}

/// Mutable sibling of [`borrow_ref`].
#[inline]
pub fn borrow_ref_mut<'a, T>(ptr: *mut T) -> &'a mut T {
    debug_assert!(!ptr.is_null(), "borrow_ref_mut: ptr must be non-null");
    // SAFETY: caller upholds the contract above; exclusive access to
    // `*ptr` for the returned lifetime.
    unsafe { &mut *ptr }
}

/// Offset a `NonNull<u8>` by `byte_offset` and return the resulting
/// `NonNull<u8>`. Folds the slab/large-alloc pattern of "skip past a
/// header at a fixed byte offset" into one helper so the kernel-heap
/// site stays in safe Rust.
///
/// # Safety contract on the caller
///
/// `base + byte_offset` must lie inside the same allocation as `base`,
/// and the returned pointer must be used only while that allocation is
/// live. The interior `unsafe` (pointer arithmetic and the
/// `NonNull::new_unchecked`) is sound under that contract.
#[inline]
pub fn nonnull_byte_offset(base: NonNull<u8>, byte_offset: usize) -> NonNull<u8> {
    // SAFETY: caller upholds the contract above; `byte_offset` is
    // non-negative and stays inside the caller-owned allocation, so
    // the result is non-null.
    unsafe {
        let raw = base.as_ptr().add(byte_offset);
        NonNull::new_unchecked(raw)
    }
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

/// Write `value` to `out` if `out` is non-null; no-op when null.

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

/// Write `value` through a raw `*mut T` if the pointer is non-null.
/// No-op when `ptr.is_null()`.
///
/// Used by syscall handlers that accept optional out-pointers from
/// userspace and need to publish a single value (e.g. `src_ip`,
/// `peer_port`). Caller upholds the module-level contract for `ptr`
/// when non-null (writable for `size_of::<T>()` bytes, aligned for `T`,
/// no aliasing borrow live).
#[inline]
pub fn write_if_non_null<T>(ptr: *mut T, value: T) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: caller upholds the module-level contract; the null
    // branch above narrows to the non-null case.
    unsafe {
        *ptr = value;
    }
}

/// Write `value` at `ptr.add(index)` using `ptr::write`. Folds
/// `unsafe { ptr.add(index).write(value) }` interior to OSTD.
#[inline]
pub fn write_at_index<T>(ptr: *mut T, index: usize, value: T) {
    // SAFETY: caller upholds the module-level contract for the
    // `[ptr, ptr + index + 1)` range.
    unsafe {
        ptr.add(index).write(value);
    }
}

/// Append a NUL terminator at `buf[len]` after copying `len` bytes
/// from `src`. Used by C-string–producing syscall paths.
///
/// Caller upholds the module-level contract for `buf` over the range
/// `[buf, buf + len + 1)`.
#[inline]
pub fn copy_with_nul_terminator(buf: *mut u8, src: &[u8], len: usize) {
    // SAFETY: caller-verified bounds; copy plus one trailing NUL.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), buf, len);
        *buf.add(len) = 0;
    }
}

/// Copy `len` bytes from `src` to `dst` using
/// `copy_nonoverlapping`. Caller upholds non-overlap + bounds.
#[inline]
pub fn copy_bytes(dst: *mut u8, src: *const u8, len: usize) {
    // SAFETY: caller upholds the module-level contract for both
    // ranges and certifies they do not overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst, len);
    }
}

/// Advance `ptr` by `byte_offset` elements (typed) — i.e.
/// `ptr.add(byte_offset)`. Caller asserts the resulting pointer
/// stays within the same allocation.
#[inline]
pub fn ptr_add<T>(ptr: *mut T, count: usize) -> *mut T {
    // SAFETY: caller asserts the in-bounds offset.
    unsafe { ptr.add(count) }
}

/// Same as [`ptr_add`] but for `*const T`.
#[inline]
pub fn ptr_add_const<T>(ptr: *const T, count: usize) -> *const T {
    // SAFETY: caller asserts the in-bounds offset.
    unsafe { ptr.add(count) }
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
