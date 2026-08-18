//! Raw-pointer-to-slice borrow primitives.
//!
//! Each helper folds one `unsafe` raw-pointer-to-slice call interior to OSTD,
//! so kernel-half code holding a pointer from an FFI boundary receives a safe
//! slice.
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

use crate::sync::init_flag::InitFlag;

/// Read a possibly-unaligned `T: Copy` at byte `offset` of `payload`.
/// Returns `None` if `offset + size_of::<T>()` exceeds `payload.len()`.
#[inline]
pub fn read_pod_at<T: Copy>(payload: &[u8], offset: usize) -> Option<T> {
    let needed = core::mem::size_of::<T>();
    if offset.checked_add(needed)? > payload.len() {
        return None;
    }
    // SAFETY: bounds-checked above; `T: Copy` permits any byte pattern and
    // `read_unaligned` lifts the alignment requirement.
    let p = unsafe { payload.as_ptr().add(offset) } as *const T;
    Some(unsafe { core::ptr::read_unaligned(p) })
}

/// Borrow `len` elements of `T` starting at `ptr` for the duration of `f`.
///
/// See the module-level safety contract.
///
/// Scoped rather than returning: a caller-chosen `'a` yields references the
/// compiler believes are unrelated — aliasing UB on the second `_mut` call,
/// with no `unsafe` in sight at the call site. A closure's argument lifetime
/// is higher-ranked, so the caller cannot name it or outlive the call.
#[inline]
pub fn with_buf<T, R>(ptr: *const T, len: usize, f: impl FnOnce(&[T]) -> R) -> R {
    // SAFETY: caller upholds the module-level contract.
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
/// For the accessors a closure cannot serve — `fn as_mut_slice(&mut self) ->
/// &mut [u8]` over a buffer the receiver owns. The lifetime is `anchor`'s
/// rather than caller-chosen, so `&mut self`'s exclusivity becomes the
/// slice's. Deliberately `&mut A` only: a shared form let callers pass a token
/// standing in no relation to `ptr`, which constrained nothing.
#[inline]
pub fn anchored_buf_mut<'a, A: ?Sized, T>(
    _anchor: &'a mut A,
    ptr: *mut T,
    len: usize,
) -> &'a mut [T] {
    // SAFETY: caller upholds the module-level contract; `anchor` bounds the
    // lifetime and its `&mut` bounds the aliasing.
    unsafe { core::slice::from_raw_parts_mut(ptr, len) }
}

/// [`anchored_buf_mut`] for a `NonNull`.
#[inline]
pub fn anchored_nonnull_mut<'a, A: ?Sized, T>(
    anchor: &'a mut A,
    ptr: NonNull<T>,
    len: usize,
) -> &'a mut [T] {
    anchored_buf_mut(anchor, ptr.as_ptr(), len)
}

/// A region handed over exactly once, to be turned into a `&'static mut [T]`.
///
/// `'static` is a boot-reserved region's real lifetime; the danger is the
/// *repetition* — a second `&'static mut` to the same bytes is aliasing UB,
/// and `'static` coerces to any shorter lifetime, so a plain function could
/// not stop a caller taking one twice. Hence the linear handle:
/// [`claim`](Self::claim) mints it only against an unset [`InitFlag`], and
/// [`into_static_mut`] consumes it by value.
pub struct OneShotBuf<T: 'static> {
    ptr: NonNull<T>,
    len: usize,
}

impl<T: 'static> OneShotBuf<T> {
    /// Claim `[ptr, ptr + len)` against `flag`, or `None` if `flag` was
    /// already claimed.
    ///
    /// `flag` must be the one flag guarding this region; reusing a flag denies
    /// the second claim, which fails closed. Beyond that, the module-level
    /// contract: `ptr` is aligned for `T` and valid for `len` consecutive `T`
    /// values that are never freed.
    #[inline]
    pub fn claim(flag: &'static InitFlag, ptr: NonNull<T>, len: usize) -> Option<Self> {
        flag.init_once().then_some(Self { ptr, len })
    }

    /// Consume the handle for the region's one `&'static mut [T]`.
    #[inline]
    pub fn into_static_mut(self) -> &'static mut [T] {
        // SAFETY: `claim` is the only constructor, succeeds once per flag, and
        // `OneShotBuf` is neither `Copy` nor `Clone`, so this is the only
        // `&'static mut` over these bytes that ever exists. Validity,
        // alignment and length are the claim-time contract.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
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

const U64_SLOTS_PER_PAGE: usize = 4096 / core::mem::size_of::<u64>();

/// View the `index`-th `u64` of the 4 KiB page at `page_base` as an
/// [`AtomicU64`](core::sync::atomic::AtomicU64) for the duration of `f`.
///
/// For memory a second agent may write between two of this program's accesses:
/// the hardware page walker stamps Accessed and Dirty into any entry it uses,
/// at any instant, so a plain load races however the kernel serialises itself
/// and a `&u64` — or a `&mut` over the enclosing table — is an exclusivity
/// claim neither the machine nor another CPU honours.
///
/// A page-aligned base plus an index bounded by the page's slot count cannot
/// leave the page. Both are asserted here, in every build.
#[inline]
pub fn with_atomic_u64_in_page<R>(
    page_base: NonNull<u64>,
    index: usize,
    f: impl FnOnce(&core::sync::atomic::AtomicU64) -> R,
) -> R {
    assert!(
        page_base.as_ptr().align_offset(4096) == 0,
        "with_atomic_u64_in_page: base must be 4 KiB aligned"
    );
    assert!(
        index < U64_SLOTS_PER_PAGE,
        "with_atomic_u64_in_page: index out of page"
    );
    // SAFETY: the asserts above put `slot` inside the page `page_base` opens,
    // which the module-level contract says is one live allocation, so the
    // offset lands on an aligned, initialised `u64`. `AtomicU64` has the same
    // layout as `u64`.
    unsafe {
        let slot = page_base.as_ptr().add(index);
        f(core::sync::atomic::AtomicU64::from_ptr(slot))
    }
}

/// Offset a `NonNull<u8>` by `byte_offset` within a region of `region_len`
/// bytes.
///
/// `region_len` turns "stays inside the allocation" from a caller promise into
/// an assertion: given a base that opens `region_len` bytes — the module-level
/// contract — an offset within them cannot escape.
///
/// # Panics
///
/// If `byte_offset > region_len`.
#[inline]
pub fn nonnull_byte_offset_in(
    base: NonNull<u8>,
    byte_offset: usize,
    region_len: usize,
) -> NonNull<u8> {
    assert!(
        byte_offset <= region_len,
        "nonnull_byte_offset_in: offset past the end of the region"
    );
    // SAFETY: the assert keeps the offset inside the `region_len` bytes the
    // module-level contract says `base` opens, so the arithmetic is in bounds
    // and the result is non-null.
    unsafe {
        let raw = base.as_ptr().add(byte_offset);
        NonNull::new_unchecked(raw)
    }
}

/// Borrow `len` elements of `T` at `(base as *mut u8) + byte_offset` for the
/// duration of `f`.
#[inline]
pub fn with_at_mut<T, R>(
    base: NonNull<u8>,
    byte_offset: usize,
    len: usize,
    f: impl FnOnce(&mut [T]) -> R,
) -> R {
    // SAFETY: caller upholds the module-level contract, so the byte arithmetic
    // stays inside the caller-owned allocation. `from_raw_parts_mut::<T>` is
    // UB on a pointer not aligned for `T`, which the debug assert traps.
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
/// For linker-section iteration, where `__start_<section>` and
/// `__stop_<section>` flank a contiguous array. `'static` is the honest
/// lifetime of a section: it is part of the image and is never freed.
#[inline]
pub fn section_slice<T: 'static>(start: *const T, stop: *const T) -> &'static [T] {
    // SAFETY: `offset_from` requires both pointers to land in the same
    // allocated object, which the linker guarantees for a `__start_*` /
    // `__stop_*` symbol pair.
    let len = unsafe { stop.offset_from(start) };
    let len = if len < 0 { 0 } else { len as usize };
    // SAFETY: module-level contract.
    unsafe { core::slice::from_raw_parts(start, len) }
}

/// Write `value` through a raw `*mut T` if the pointer is non-null.
/// No-op when `ptr.is_null()`.
///
/// Caller upholds the module-level contract for `ptr` when non-null.
#[inline]
pub fn write_if_non_null<T>(ptr: *mut T, value: T) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: caller upholds the module-level contract; the branch above
    // narrows to the non-null case.
    unsafe {
        *ptr = value;
    }
}

/// Write `value` at `ptr.add(index)` using `ptr::write`.
#[inline]
pub fn write_at_index<T>(ptr: *mut T, index: usize, value: T) {
    // SAFETY: caller upholds the module-level contract for the
    // `[ptr, ptr + index + 1)` range.
    unsafe {
        ptr.add(index).write(value);
    }
}

/// Write `value` at the kernel-virtual byte address `addr`.
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
    // SAFETY: caller upholds the per-helper contract above.
    unsafe { core::ptr::write(p, value) };
}

/// Zero `len` bytes starting at the kernel-virtual byte address `addr`.
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

/// Append a NUL terminator at `buf[len]` after copying `len` bytes from `src`.
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
    // SAFETY: caller upholds the module-level contract for both ranges and
    // certifies they do not overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst, len);
    }
}

/// Typed pointer advance. Caller asserts the resulting pointer stays within
/// the same allocation.
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
        // A bare `[u8; 16]` carries 1-byte alignment, so the allocator may
        // place it where `from_raw_parts_mut::<u32>` would be UB.
        #[repr(align(4))]
        struct AlignedBuf([u8; 16]);
        let mut wrap = AlignedBuf([0u8; 16]);
        let base = NonNull::new(wrap.0.as_mut_ptr()).unwrap();
        with_at_mut::<u32, _>(base, 4, 2, |view| {
            view[0] = 0xdeadbeefu32;
            view[1] = 0x1234_5678u32;
        });
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
