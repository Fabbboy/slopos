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
//! - the returned borrow's lifetime — the anchor's, or `'static` — does not
//!   outlive the underlying storage,
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

/// Borrow `len` elements of `T` starting at `ptr` for the duration of `f`.
///
/// Companion of `core::slice::from_raw_parts`. See module-level
/// safety contract.
///
/// # Why scoped rather than returning
///
/// A `fn(*const T, usize) -> &'a [T]` lets the *caller* pick `'a`, so two
/// calls yield two references the compiler believes are unrelated — and for
/// the `_mut` forms that is instant aliasing UB on the second call, with no
/// `unsafe` block anywhere in sight at the call site. A closure's argument
/// lifetime is higher-ranked: the caller cannot name it, so it cannot choose
/// it, and the borrow cannot outlive the call.
#[inline]
pub fn with_buf<T, R>(ptr: *const T, len: usize, f: impl FnOnce(&[T]) -> R) -> R {
    // SAFETY: caller upholds the module-level contract; this is the
    // single `from_raw_parts` call site for all kernel-half consumers
    // of the `*const T + len` pattern.
    f(unsafe { core::slice::from_raw_parts(ptr, len) })
}

/// Mutable sibling of [`with_buf`].
#[inline]
pub fn with_buf_mut<T, R>(ptr: *mut T, len: usize, f: impl FnOnce(&mut [T]) -> R) -> R {
    // SAFETY: caller upholds the module-level contract, including
    // exclusive ownership of `[ptr, ptr+len)` for the duration of `f`.
    f(unsafe { core::slice::from_raw_parts_mut(ptr, len) })
}

/// Borrow `len` elements of `T` at `ptr` for as long as `anchor` lives.
///
/// The other answer to check 8's shape, for the callers a closure cannot
/// serve: an accessor that *returns* a reference, like
/// `fn as_slice(&self) -> &[u8]` over a buffer the receiver owns. Here the
/// lifetime is not fabricated and not caller-chosen — it is `anchor`'s, so the
/// caller has to present something that genuinely outlives the borrow, and at
/// such an accessor that something is `&self`.
///
/// A token anchor is the degenerate case. `&()`, or a reference to the caller's
/// own `len` local, bounds the borrow to the caller's frame — honest, and
/// machine-checked — but it stands in no relation to `ptr`, so it constrains
/// nothing else: that the mapping stays valid across that frame remains
/// entirely the caller's assertion.
///
/// # Safety contract on the caller
///
/// The module-level contract, plus: the mapping at `ptr` must stay valid and
/// unaliased for at least as long as `anchor`.
#[inline]
pub fn anchored_buf<'a, A: ?Sized, T>(_anchor: &'a A, ptr: *const T, len: usize) -> &'a [T] {
    // SAFETY: caller upholds the contract above; `anchor` bounds the lifetime.
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

/// Mutable sibling of [`anchored_buf`]. Takes `&mut A`, so the exclusivity of
/// the returned slice is the exclusivity of the anchor.
#[inline]
pub fn anchored_buf_mut<'a, A: ?Sized, T>(
    _anchor: &'a mut A,
    ptr: *mut T,
    len: usize,
) -> &'a mut [T] {
    // SAFETY: caller upholds the contract above; `anchor` bounds the lifetime
    // and its `&mut` bounds the aliasing.
    unsafe { core::slice::from_raw_parts_mut(ptr, len) }
}

/// [`anchored_buf`] for a `NonNull`.
#[inline]
pub fn anchored_nonnull<'a, A: ?Sized, T>(anchor: &'a A, ptr: NonNull<T>, len: usize) -> &'a [T] {
    anchored_buf(anchor, ptr.as_ptr().cast_const(), len)
}

/// Mutable sibling of [`anchored_nonnull`].
#[inline]
pub fn anchored_nonnull_mut<'a, A: ?Sized, T>(
    anchor: &'a mut A,
    ptr: NonNull<T>,
    len: usize,
) -> &'a mut [T] {
    anchored_buf_mut(anchor, ptr.as_ptr(), len)
}

/// Take a `&'static mut [T]` over a region installed exactly once and never
/// freed.
///
/// The escape hatch for a one-shot install — a boot-reserved table handed to
/// the structure that owns it for the rest of the machine's life. `'static` is
/// what such a region's lifetime actually is. It buys honesty, not safety:
/// `'static` coerces to any shorter lifetime at the call site, so it does not
/// stop a caller picking one and then picking it again.
///
/// # Safety contract on the caller
///
/// The module-level contract, plus: the region is never freed, and this is
/// called **once** for it. A second call would hand out a second
/// `&'static mut` to the same bytes, which is aliasing UB — the one-shot
/// property has to come from the caller's own state machine, and there is no
/// way to express it here.
#[inline]
pub fn install_buf_mut<T: 'static>(ptr: *mut T, len: usize) -> &'static mut [T] {
    // SAFETY: caller upholds the contract above.
    unsafe { core::slice::from_raw_parts_mut(ptr, len) }
}

/// [`with_buf`] for a `NonNull`, with the null-pointer branch elided.
#[inline]
pub fn with_nonnull<T, R>(ptr: NonNull<T>, len: usize, f: impl FnOnce(&[T]) -> R) -> R {
    // SAFETY: `NonNull::as_ptr` returns a non-null pointer; remaining
    // preconditions are the module-level contract.
    f(unsafe { core::slice::from_raw_parts(ptr.as_ptr(), len) })
}

/// Mutable sibling of [`with_nonnull`].
#[inline]
pub fn with_nonnull_mut<T, R>(ptr: NonNull<T>, len: usize, f: impl FnOnce(&mut [T]) -> R) -> R {
    // SAFETY: `NonNull::as_ptr` returns a non-null pointer; remaining
    // preconditions are the module-level contract.
    f(unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), len) })
}

/// Borrow a single `T` value at `ptr` for the duration of `f`. Companion of
/// Borrow a single `T` at `ptr` for as long as `anchor` lives.
///
/// [`anchored_buf`]'s single-value sibling, for accessors that return a
/// reference rather than consuming one.
///
/// # Safety contract on the caller
///
/// `ptr` must be non-null, aligned for `T`, point at an initialised `T`,
/// and stay valid and unaliased for at least as long as `anchor`.
#[inline]
pub fn anchored_ref<'a, A: ?Sized, T>(_anchor: &'a A, ptr: *const T) -> &'a T {
    debug_assert!(!ptr.is_null(), "anchored_ref: ptr must be non-null");
    // SAFETY: caller upholds the contract above; `anchor` bounds the lifetime.
    unsafe { &*ptr }
}

/// View the `index`-th `u64` at `base` as an [`AtomicU64`] for the duration of
/// `f`.
///
/// For memory a second agent may write between two of this program's accesses.
/// A page-table entry is the motivating case: the hardware page walker stamps
/// Accessed and Dirty into any entry it uses, at any instant, so a plain load
/// is a race however the kernel serialises itself — and forming a `&u64`, or a
/// `&mut` over the enclosing table, would be an exclusivity claim neither the
/// machine nor another CPU honours. An atomic access is never torn, never
/// coalesced with a neighbour, and never elided; and the shared `&AtomicU64`
/// composes with itself, so two CPUs touching two slots of one table is not a
/// claim either of them has to defend.
///
/// # Safety contract on the caller
///
/// `base.add(index)` must lie inside one live allocation and be 8-byte
/// aligned.
#[inline]
pub fn with_atomic_u64_at<R>(
    base: *mut u64,
    index: usize,
    f: impl FnOnce(&core::sync::atomic::AtomicU64) -> R,
) -> R {
    debug_assert!(!base.is_null(), "with_atomic_u64_at: base must be non-null");
    debug_assert!(
        base.align_offset(align_of::<u64>()) == 0,
        "with_atomic_u64_at: base must be 8-byte aligned"
    );
    // SAFETY: caller upholds the contract above, so `slot` addresses an
    // initialised, aligned `u64` inside a live allocation for the duration
    // of `f`. `AtomicU64` has the same layout as `u64`.
    unsafe {
        let slot = base.add(index);
        f(core::sync::atomic::AtomicU64::from_ptr(slot))
    }
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

/// Borrow `len` elements of `T` at `(base as *mut u8) + byte_offset` for the
/// duration of `f`. Folds the kernel-heap pattern of "advance a NonNull by a
/// byte offset, then take a typed slice of `len` Ts" into one call.
///
/// Used in `mm/src/kernel_heap.rs` to view a slab object's body or a
/// large-alloc body that lives at a fixed byte offset past a header
/// pointer.
#[inline]
pub fn with_at_mut<T, R>(
    base: NonNull<u8>,
    byte_offset: usize,
    len: usize,
    f: impl FnOnce(&mut [T]) -> R,
) -> R {
    // SAFETY: caller upholds the module-level contract; `base` is
    // NonNull and the byte arithmetic stays inside the caller-owned
    // allocation. Alignment for `T` is asserted below — Miri-detected
    // soundness gap: `from_raw_parts_mut::<T>` is UB on a pointer that
    // is not aligned for `T`, so we trap the buggy-caller case in
    // debug builds rather than silently producing an unaligned slice.
    let slice = unsafe {
        let typed_ptr = base.as_ptr().add(byte_offset) as *mut T;
        debug_assert!(
            (typed_ptr as usize) % core::mem::align_of::<T>() == 0,
            "with_at_mut::<T>: typed pointer is not aligned for T"
        );
        core::slice::from_raw_parts_mut(typed_ptr, len)
    };
    f(slice)
}

/// Slice between two `*const T` anchors. Returns `&start[..(stop -
/// start)]`. If `stop <= start`, returns an empty slice rather than
/// panicking.
///
/// Used for linker-section iteration where the linker exports
/// `__start_<section>` and `__stop_<section>` symbols flanking a
/// contiguous array. Absorbs both the `*const T` pointer arithmetic
/// (`stop.offset_from(start)`) and the slice construction.
///
/// `'static` because that is the honest lifetime of a linker section: it is
/// part of the image and is never freed. A caller-chosen `'a` would have let
/// two calls hand out two references the compiler believed were unrelated.
#[inline]
pub fn section_slice<T: 'static>(start: *const T, stop: *const T) -> &'static [T] {
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

/// Write `value` at the kernel-virtual byte address `addr`. Folds
/// `unsafe { core::ptr::write(addr as *mut T, value) }` interior to
/// OSTD for the common kernel-stack-init pattern, where the caller
/// has just freshly allocated the destination region and no other CPU
/// can observe it yet.
///
/// The caller must ensure:
///
/// - `addr` is a valid kernel-virtual byte address aligned for `T`,
/// - the region `[addr, addr + size_of::<T>())` is owned exclusively
///   by the caller for the duration of the write,
/// - no other reference into that region is live.
#[inline]
pub fn write_kernel_va<T>(addr: u64, value: T) {
    let p = addr as *mut T;
    // SAFETY: caller upholds the per-helper contract above. Used
    // during task-create kernel-stack priming where the stack frame
    // is freshly allocated and no observer is yet attached.
    unsafe { core::ptr::write(p, value) };
}

/// Zero `len` bytes starting at the kernel-virtual byte address
/// `addr`. Folds `unsafe { core::ptr::write_bytes(addr, 0, len) }`
/// interior to OSTD for the freshly-allocated-region zeroing pattern
/// (task-stack hygiene, kernel-stack zero-fill).
///
/// The caller must ensure `addr` is a valid kernel-virtual byte
/// address, that the byte range `[addr, addr + len)` is owned
/// exclusively for the duration of the call, and that no live
/// reference into that range is held elsewhere.
#[inline]
pub fn zero_bytes_at_kernel_va(addr: u64, len: usize) {
    let p = addr as *mut u8;
    // SAFETY: caller upholds the per-helper contract above.
    unsafe { core::ptr::write_bytes(p, 0, len) };
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
    fn with_buf_round_trip() {
        let src = [10u32, 20, 30, 40];
        with_buf::<u32, _>(src.as_ptr(), src.len(), |view| {
            assert_eq!(view, &src[..]);
        });
    }

    #[test]
    fn with_buf_mut_writes_visible_through_source() {
        let mut src = [0u32; 4];
        let ptr = src.as_mut_ptr();
        with_buf_mut::<u32, _>(ptr, 4, |view| {
            view[0] = 0xaa;
            view[3] = 0xbb;
        });
        assert_eq!(src, [0xaa, 0, 0, 0xbb]);
    }

    #[test]
    fn with_nonnull_round_trip() {
        let src = [1u8, 2, 3];
        let ptr = NonNull::new(src.as_ptr() as *mut u8).unwrap();
        with_nonnull::<u8, _>(ptr, src.len(), |view| {
            assert_eq!(view, &src[..]);
        });
    }

    #[test]
    fn with_nonnull_mut_writes_visible_through_source() {
        let mut src = [0u8; 3];
        let ptr = NonNull::new(src.as_mut_ptr()).unwrap();
        with_nonnull_mut::<u8, _>(ptr, 3, |view| view.fill(0x42));
        assert_eq!(src, [0x42; 3]);
    }

    #[test]
    fn with_at_mut_advances_by_byte_offset() {
        // Use a u32-aligned backing storage so the `&mut [u32]` view at
        // byte_offset 4 is sound. A bare `[u8; 16]` only carries 1-byte
        // alignment at the type level and Miri's allocator may hand back
        // a buffer that lands on a 2-byte boundary, which would violate
        // `from_raw_parts_mut::<u32>`'s alignment requirement.
        #[repr(align(4))]
        struct AlignedBuf([u8; 16]);
        let mut wrap = AlignedBuf([0u8; 16]);
        let base = NonNull::new(wrap.0.as_mut_ptr()).unwrap();
        with_at_mut::<u32, _>(base, 4, 2, |view| {
            view[0] = 0xdeadbeefu32;
            view[1] = 0x1234_5678u32;
        });
        // First 4 bytes untouched, then u32 LE/BE pattern.
        assert_eq!(&wrap.0[0..4], &[0, 0, 0, 0]);
        assert_eq!(&wrap.0[4..8], &0xdeadbeefu32.to_ne_bytes());
        assert_eq!(&wrap.0[8..12], &0x1234_5678u32.to_ne_bytes());
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
