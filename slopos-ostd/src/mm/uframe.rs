//! Untyped frames and segments — byte-copy-only physical memory.
//!
//! `UFrame<M>` and `USegment<M>` expose physical memory through a
//! deliberately-restricted byte-copy interface so user, peripheral,
//! and DMA-tampered memory can never be observed as a Rust
//! reference (`&T` / `&[u8]`).
//!
//! # No-reference discipline
//!
//! `UFrame` and `USegment` deliberately do **not** implement
//! `Deref`, `DerefMut`, `AsRef`, `AsMut`, `Index`, `IndexMut`, nor
//! expose `as_slice` / `as_mut_slice`. The `compile_fail` doctests
//! on [`UFrame`] lock this in. Any future addition that returns
//! `&T` or `&[u8]` from these types is a soundness regression and
//! must be rejected at review.

use core::marker::PhantomData;
use core::mem::{align_of, size_of};

use crate::{AllocError, KVec};

use crate::mm::frame::{
    AnonymousMeta, AnyFrameMeta, Frame, FrameError, Paddr, PageTableMeta as _PageTableMeta,
    RingMeta,
};
use crate::mm::phys;
use crate::mm::pod::Pod;

const PAGE_SIZE: usize = 4096;

/// Byte-copy error type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UFrameError {
    /// `offset + len` exceeds the region's byte length.
    OutOfBounds,
    /// `(paddr + offset) % align_of::<T>() != 0` for a `read_pod` /
    /// `write_pod` call.
    Misaligned,
    /// A vectored byte-copy completed only part of the requested
    /// transfer. Reserved for future vectored-I/O / partial-segment
    /// paths; unused by the current byte-copy methods.
    Truncated,
    /// `USegment` construction tried to allocate per-segment
    /// bookkeeping and the kernel heap refused.
    OutOfMemory,
}

impl From<AllocError> for UFrameError {
    fn from(_: AllocError) -> Self {
        Self::OutOfMemory
    }
}

// ---------------------------------------------------------------------------
// AnyUFrameMeta marker.
// ---------------------------------------------------------------------------

/// Marker trait identifying frame-metadata types whose pages are
/// untyped — i.e. their contents may be tampered with by user code,
/// peripherals, or DMA, and therefore must only be reached through
/// the byte-copy interface.
///
/// # Safety
///
/// Implementor opts a metadata type into the untyped category. The
/// implementation must guarantee that no `&T` reference is ever
/// constructed pointing into a frame of this metadata type from
/// anywhere outside `slopos-ostd::mm::uframe`. Sensitive
/// kernel-owned metadata types (e.g. `PageTableMeta`, `KernelMeta`)
/// MUST NOT implement this trait.
pub unsafe trait AnyUFrameMeta: AnyFrameMeta + Default {}

// SAFETY: `AnonymousMeta` is a ZST representing user/anon untyped
// pages — exactly the case `AnyUFrameMeta` describes. `KernelMeta`
// and `PageTableMeta` deliberately do **not** implement this trait
// because their pages are sensitive kernel-owned memory.
unsafe impl AnyUFrameMeta for AnonymousMeta {}

// SAFETY: `RingMeta` backs a SlopRing SQ/CQ region that is mapped
// read+write into userspace and mutated concurrently by a user thread
// — exactly the untyped case `AnyUFrameMeta` describes. The kernel ring
// code reaches the bytes only through this module's byte-copy /
// volatile interface, never forming a `&Sqe` / `&mut Cqe` over the
// frame (SLOPRING § 5.2 / § 5.3, AD-3 / Inv. 4/5).
unsafe impl AnyUFrameMeta for RingMeta {}

// Reference the import so the unused-import lint doesn't fire on
// `_PageTableMeta`; documenting why it is *not* `AnyUFrameMeta`.
const _SENSITIVE: PhantomData<_PageTableMeta> = PhantomData;

// ---------------------------------------------------------------------------
// Bounds + alignment helpers.
// ---------------------------------------------------------------------------

#[inline]
pub(crate) fn check_range(offset: usize, len: usize, region: usize) -> Result<(), UFrameError> {
    let end = offset.checked_add(len).ok_or(UFrameError::OutOfBounds)?;
    if end > region {
        return Err(UFrameError::OutOfBounds);
    }
    Ok(())
}

#[inline]
fn check_alignment<T: Pod>(paddr: Paddr, offset: usize) -> Result<(), UFrameError> {
    let align = align_of::<T>();
    let abs = (paddr.as_u64() as usize).wrapping_add(offset);
    if abs % align != 0 {
        return Err(UFrameError::Misaligned);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// UFrame: single untyped frame.
// ---------------------------------------------------------------------------

/// A single physical 4 KiB frame whose contents are untyped — i.e.
/// reachable only through the byte-copy interface defined on this
/// type. Holds one ref to the underlying [`Frame<M>`] for as long as
/// it lives. See module docs for the no-reference discipline.
///
/// ## No-reference discipline (compile-fail tests)
///
/// `UFrame` deliberately does **not** implement `Deref`,
/// `Index<Range<usize>>`, or expose `as_slice`. Each of the
/// following must fail to compile; if any of them ever start
/// passing, a soundness invariant has been broken.
///
/// `Deref`:
/// ```compile_fail
/// use core::ops::Deref;
/// use slopos_ostd::mm::frame::AnonymousMeta;
/// use slopos_ostd::mm::uframe::UFrame;
/// let f: UFrame<AnonymousMeta> = unimplemented!();
/// let _ = f.deref();
/// ```
///
/// `Index<Range<usize>>` / `&uframe[..]`:
/// ```compile_fail
/// use slopos_ostd::mm::frame::AnonymousMeta;
/// use slopos_ostd::mm::uframe::UFrame;
/// let f: UFrame<AnonymousMeta> = unimplemented!();
/// let _: &[u8] = &f[0..4];
/// ```
///
/// `as_slice`:
/// ```compile_fail
/// use slopos_ostd::mm::frame::AnonymousMeta;
/// use slopos_ostd::mm::uframe::UFrame;
/// let f: UFrame<AnonymousMeta> = unimplemented!();
/// let _: &[u8] = f.as_slice();
/// ```
pub struct UFrame<M: AnyUFrameMeta = AnonymousMeta>(Frame<M>);

impl UFrame<AnonymousMeta> {
    /// Wrap a freshly-allocated 4 KiB user paddr that this `UFrame`
    /// will own through META_SLOTS. The first call for a paddr does
    /// `from_unused` (slot UNUSED → TYPED, ref count = 1). Subsequent
    /// calls for the same paddr (e.g. fork(2)'s child mapping the
    /// parent's pages) fall through to `from_in_use`, bumping the
    /// existing slot's ref count.
    ///
    /// `Drop` of the LAST wrapper for a paddr returns the page to the
    /// registered [`FrameAlloc`] — exactly the legacy
    /// `free_page_frame` semantics, but driven by META_SLOTS rather
    /// than the legacy refcount table.
    pub fn wrap_user_paddr(paddr: Paddr) -> Result<Self, FrameError> {
        match Frame::<AnonymousMeta>::from_unused(paddr, AnonymousMeta::default()) {
            Ok(frame) => Ok(Self(frame)),
            Err(FrameError::StateMismatch) => Ok(Self(Frame::<AnonymousMeta>::from_in_use(paddr)?)),
            Err(e) => Err(e),
        }
    }
}

impl UFrame<RingMeta> {
    /// Allocate a single zeroed physical frame from the registered
    /// frame allocator and wrap it as an untyped `UFrame<RingMeta>`.
    ///
    /// This is the SlopRing allocation entry point (SLOPRING § 5.1): the
    /// ring crate owns the returned `UFrame` for the ring's lifetime,
    /// reads SQEs / writes CQEs through the volatile byte-copy interface,
    /// and maps the same physical frame into the owning process. Returns
    /// `None` if no allocator is registered or the buddy is exhausted.
    pub fn alloc() -> Option<Self> {
        let alloc = crate::mm::frame_alloc::current_frame_allocator()?;
        let opts = crate::mm::frame::FrameAllocOptions::single().zeroed();
        let paddr = alloc.alloc(opts)?;
        match Frame::<RingMeta>::from_unused(paddr, RingMeta::default()) {
            Ok(frame) => Some(Self(frame)),
            Err(_) => {
                alloc.dealloc(paddr, 1);
                None
            }
        }
    }
}

impl<M: AnyUFrameMeta> UFrame<M> {
    /// Wrap a freshly-allocated, currently-unused physical frame and
    /// install `meta` into its slot. Mirrors
    /// [`Frame::from_unused`] but yields a no-reference handle.
    pub fn from_unused(paddr: Paddr, meta: M) -> Result<Self, FrameError> {
        Ok(Self(Frame::<M>::from_unused(paddr, meta)?))
    }

    /// Borrow an already-live untyped frame, bumping its
    /// ref-count. The slot's `M` must already match the wrapper's
    /// `M`; mismatches return [`FrameError::StateMismatch`].
    pub fn from_in_use(paddr: Paddr) -> Result<Self, FrameError> {
        Ok(Self(Frame::<M>::from_in_use(paddr)?))
    }

    /// Physical address of the underlying frame.
    pub fn paddr(&self) -> Paddr {
        self.0.paddr()
    }

    /// Current ref count of the underlying frame slot.
    pub fn reference_count(&self) -> u32 {
        self.0.reference_count()
    }

    /// Internal: drop the untyped wrapper and yield the inner
    /// [`Frame<M>`]. Crate-private to keep the no-reference
    /// discipline — exposing this publicly would let callers go
    /// `uframe.into_frame().borrow()` and read the metadata, which
    /// is fine, but would also let them call any future helper on
    /// `Frame<M>` that returns `&[u8]` over frame contents. Keep
    /// the exit door inside the crate.
    #[allow(dead_code)]
    pub(crate) fn into_frame(self) -> Frame<M> {
        self.0
    }

    /// Internal: wrap an existing `Frame<M>` as untyped. Crate-private.
    #[allow(dead_code)]
    pub(crate) fn from_frame(frame: Frame<M>) -> Self {
        Self(frame)
    }

    /// Copy `dst.len()` bytes from `[paddr + offset, paddr + offset
    /// + dst.len())` into `dst`.
    pub fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> Result<(), UFrameError> {
        check_range(offset, dst.len(), PAGE_SIZE)?;
        if dst.is_empty() {
            return Ok(());
        }
        // SAFETY: `phys_to_virt(self.paddr())` returns a kernel-virt
        // pointer into a frame this `UFrame` owns (ref-count > 0
        // guarantees the slot is live for the duration of `&self`);
        // `offset + dst.len() <= PAGE_SIZE` was just checked, so
        // `[src, src + dst.len())` stays within the frame; `dst` is
        // a Rust `&mut [u8]` so it cannot alias the kernel-virt
        // mapping for a `UFrame` (those mappings only address the
        // physical frame, not arbitrary kernel buffers).
        unsafe {
            let src = phys::phys_to_virt(self.paddr()).add(offset);
            core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len());
        }
        Ok(())
    }

    /// Copy `src.len()` bytes from `src` into `[paddr + offset,
    /// paddr + offset + src.len())`.
    pub fn write_bytes(&self, offset: usize, src: &[u8]) -> Result<(), UFrameError> {
        check_range(offset, src.len(), PAGE_SIZE)?;
        if src.is_empty() {
            return Ok(());
        }
        // SAFETY: same invariant as `read_bytes`, swap roles.
        unsafe {
            let dst = phys::phys_to_virt(self.paddr()).add(offset);
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
        }
        Ok(())
    }

    /// Read a `Pod` value from `paddr + offset`. Requires
    /// `(paddr + offset) % align_of::<T>() == 0`.
    pub fn read_pod<T: Pod>(&self, offset: usize) -> Result<T, UFrameError> {
        check_range(offset, size_of::<T>(), PAGE_SIZE)?;
        check_alignment::<T>(self.paddr(), offset)?;
        // SAFETY: `T: Pod` — every byte pattern of length
        // `size_of::<T>()` is a valid `T`. Range + alignment
        // checked. The pointer is live for the duration of `&self`.
        let val = unsafe {
            let src = phys::phys_to_virt(self.paddr()).add(offset) as *const T;
            core::ptr::read(src)
        };
        Ok(val)
    }

    /// Write a `Pod` value to `paddr + offset`. Requires
    /// `(paddr + offset) % align_of::<T>() == 0`.
    pub fn write_pod<T: Pod>(&self, offset: usize, value: T) -> Result<(), UFrameError> {
        check_range(offset, size_of::<T>(), PAGE_SIZE)?;
        check_alignment::<T>(self.paddr(), offset)?;
        // SAFETY: same invariant as `read_pod`.
        unsafe {
            let dst = phys::phys_to_virt(self.paddr()).add(offset) as *mut T;
            core::ptr::write(dst, value);
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Volatile / atomic accessors (SlopRing — SLOPRING § 3, § 5.3).
    //
    // The plain byte-copy methods above (`read_bytes`/`read_pod` …)
    // assume the frame's bytes do not change underneath the kernel for
    // the duration of the access. That assumption is *false* for memory
    // shared read+write with a concurrently-running userspace thread —
    // an io_uring-style ring is exactly that. A non-atomic, non-volatile
    // read racing a userspace write is a data race (UB) in the
    // Rust/LLVM abstract machine *regardless* of the value-level
    // snapshot argument, and `core::ptr::read` emits no fence so LLVM
    // may reorder reads of the SQE body relative to the `sq_tail` index
    // read even on x86.
    //
    // These four accessors are the fix: they perform `read_volatile` /
    // `write_volatile` (so the access is never elided or duplicated and
    // races are well-defined per the "atomic-or-volatile" relaxation
    // LLVM gives volatile) plus an explicit acquire/release fence on the
    // index ops, mirroring Linux's `READ_ONCE`/`smp_load_acquire` on the
    // ring head/tail. They are the single new piece of OSTD `unsafe`
    // SlopRing requires, and are in the KernMiri scope (SLOPRING § 15).
    // -----------------------------------------------------------------

    /// Volatile + acquire load of a `u32` at `offset`. Used by the
    /// kernel ring code to read a user-written index (`sq_tail` /
    /// `cq_head`) without a data race or reordering against the
    /// subsequent body reads. Requires 4-byte alignment.
    pub fn load_u32_acquire(&self, offset: usize) -> Result<u32, UFrameError> {
        check_range(offset, size_of::<u32>(), PAGE_SIZE)?;
        check_alignment::<u32>(self.paddr(), offset)?;
        // SAFETY (Inv. 4/5): `phys_to_virt(self.paddr())` points into a
        // frame this `UFrame` owns (ref-count > 0 for the life of
        // `&self`); `offset + 4 <= PAGE_SIZE` and 4-byte alignment were
        // just checked, so the read stays in-bounds and is naturally
        // aligned. `read_volatile` makes a concurrent user write
        // well-defined (no data-race UB, never elided/duplicated); the
        // following `Acquire` fence orders this load before any later
        // body read, matching the ring's release-store-before-publish
        // ABI. No `&T` is ever formed over the untyped ring frame.
        let val = unsafe {
            let src = phys::phys_to_virt(self.paddr()).add(offset) as *const u32;
            core::ptr::read_volatile(src)
        };
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        Ok(val)
    }

    /// Release + volatile store of a `u32` at `offset`. Used to publish
    /// a kernel-owned index (`sq_head` / `cq_tail`) into the shared
    /// page after the corresponding body writes. Requires 4-byte
    /// alignment.
    pub fn store_u32_release(&self, offset: usize, value: u32) -> Result<(), UFrameError> {
        check_range(offset, size_of::<u32>(), PAGE_SIZE)?;
        check_alignment::<u32>(self.paddr(), offset)?;
        // SAFETY (Inv. 4/5): same ownership + bounds + alignment
        // argument as `load_u32_acquire`. The `Release` fence orders
        // every preceding body write (e.g. the CQE just copied in)
        // before this index publication, matching the ABI userspace
        // reads with an acquire load. `write_volatile` makes the store
        // well-defined against a concurrent user reader. No `&mut T` is
        // ever formed over the untyped ring frame.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        unsafe {
            let dst = phys::phys_to_virt(self.paddr()).add(offset) as *mut u32;
            core::ptr::write_volatile(dst, value);
        }
        Ok(())
    }

    /// Volatile byte-copy *out* of the ring into `dst` (a private
    /// kernel buffer). The volatile read makes a concurrent userspace
    /// write of these bytes well-defined; the caller validates and acts
    /// only on the returned snapshot (SLOPRING § 5.3 / § 13.3).
    pub fn copy_out_volatile(&self, offset: usize, dst: &mut [u8]) -> Result<(), UFrameError> {
        check_range(offset, dst.len(), PAGE_SIZE)?;
        if dst.is_empty() {
            return Ok(());
        }
        // SAFETY (Inv. 4/5): `src` points into a frame this `UFrame`
        // owns; `offset + dst.len() <= PAGE_SIZE` checked. Each byte is
        // copied with `read_volatile`, so a concurrent user write is a
        // well-defined volatile race rather than UB and the compiler
        // cannot elide/duplicate the reads. `dst` is a Rust `&mut [u8]`
        // that cannot alias the frame mapping. No `&T` over the frame.
        unsafe {
            let base = phys::phys_to_virt(self.paddr()).add(offset);
            for (i, b) in dst.iter_mut().enumerate() {
                *b = core::ptr::read_volatile(base.add(i));
            }
        }
        Ok(())
    }

    /// Volatile byte-copy *in* from `src` (a private kernel buffer)
    /// into the ring. The volatile write makes a concurrent userspace
    /// read of these bytes well-defined.
    pub fn copy_in_volatile(&self, offset: usize, src: &[u8]) -> Result<(), UFrameError> {
        check_range(offset, src.len(), PAGE_SIZE)?;
        if src.is_empty() {
            return Ok(());
        }
        // SAFETY (Inv. 4/5): same ownership + bounds argument as
        // `copy_out_volatile`, roles swapped. Each byte is written with
        // `write_volatile`, so a concurrent user read is well-defined.
        // `src` is a Rust `&[u8]` that cannot alias the frame mapping.
        // No `&mut T` over the frame.
        unsafe {
            let base = phys::phys_to_virt(self.paddr()).add(offset);
            for (i, b) in src.iter().enumerate() {
                core::ptr::write_volatile(base.add(i), *b);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// USegment: contiguous run of UFrames.
// ---------------------------------------------------------------------------

/// A contiguous run of [`UFrame<M>`]s totalling `len_pages * 4096`
/// bytes. Internally holds one ref per frame in a `KVec`; on Drop,
/// each frame's ref is released, returning the run to the
/// allocator.
///
/// The byte-copy methods address the run as a single byte buffer
/// from offset `0` to `len_bytes() = len_pages * PAGE_SIZE`.
/// Crossing a 4 KiB boundary inside a single `read_bytes` is fine
/// because the run is physically contiguous (the kernel-virt window
/// established by [`crate::mm::phys::phys_to_virt`] is HHDM-style
/// linear).
pub struct USegment<M: AnyUFrameMeta = AnonymousMeta> {
    /// One [`Frame<M>`] ref per page. Held by the segment so that
    /// dropping the segment releases each frame's slot in turn.
    /// Only read via its `Drop` glue, so tagged `#[allow(dead_code)]`.
    #[allow(dead_code)]
    frames: KVec<Frame<M>>,
    head_paddr: Paddr,
    len_pages: usize,
    _marker: PhantomData<M>,
}

impl<M: AnyUFrameMeta> USegment<M> {
    /// Build a segment from a freshly-allocated run of `len_pages`
    /// consecutive unused frames starting at `head`, installing
    /// `M::default()` as metadata for each. On any per-frame
    /// allocation failure, frames already installed are dropped
    /// (releasing their slots) and an `Err` is returned.
    ///
    /// Test-only for now; the production allocator will route
    /// through a registered [`FrameAlloc`] with a multi-page
    /// `FrameAllocOptions` request.
    ///
    /// [`FrameAlloc`]: crate::mm::frame::FrameAlloc
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn from_unused_run(head: Paddr, len_pages: usize) -> Result<Self, UFrameError> {
        Self::from_unused_run_inner(head, len_pages)
    }

    #[allow(dead_code)]
    pub(crate) fn from_unused_run_inner(
        head: Paddr,
        len_pages: usize,
    ) -> Result<Self, UFrameError> {
        let mut frames: KVec<Frame<M>> = KVec::with_capacity(len_pages)?;
        for i in 0..len_pages {
            let paddr = Paddr::new(head.as_u64() + (i * PAGE_SIZE) as u64);
            match Frame::<M>::from_unused(paddr, M::default()) {
                Ok(f) => {
                    if frames.push(f).is_err() {
                        return Err(UFrameError::OutOfMemory);
                    }
                }
                Err(_) => {
                    // Dropping `frames` releases every successfully
                    // installed slot; no leak.
                    return Err(UFrameError::OutOfMemory);
                }
            }
        }
        Ok(Self {
            frames,
            head_paddr: head,
            len_pages,
            _marker: PhantomData,
        })
    }

    /// Physical address of the run's first frame.
    pub fn head_paddr(&self) -> Paddr {
        self.head_paddr
    }

    /// Number of pages in the run.
    pub fn len_pages(&self) -> usize {
        self.len_pages
    }

    /// Total byte length of the run (`len_pages * PAGE_SIZE`).
    pub fn len_bytes(&self) -> usize {
        self.len_pages * PAGE_SIZE
    }

    /// Read-only single-element vectored-I/O descriptor for the
    /// run. Always one slice (segments are physically contiguous);
    /// the single-element array shape is future-proofing for
    /// scatter/gather lists.
    pub fn io_slices(&self) -> [UIoSlice; 1] {
        [UIoSlice {
            paddr: self.head_paddr,
            len: self.len_bytes(),
        }]
    }

    /// Mutable single-element vectored-I/O descriptor for the run.
    pub fn io_slices_mut(&mut self) -> [UIoSliceMut; 1] {
        [UIoSliceMut {
            paddr: self.head_paddr,
            len: self.len_bytes(),
        }]
    }

    pub fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> Result<(), UFrameError> {
        check_range(offset, dst.len(), self.len_bytes())?;
        if dst.is_empty() {
            return Ok(());
        }
        // SAFETY: HHDM-style linear mapping — bytes at
        // `phys_to_virt(head) + offset` cover the entire physically
        // contiguous run. `offset + dst.len() <= len_bytes()` was
        // just checked. `dst` is a Rust slice that cannot alias the
        // frame mapping. `frames` retains a live ref per page for
        // the lifetime of `&self`.
        unsafe {
            let src = phys::phys_to_virt(self.head_paddr).add(offset);
            core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len());
        }
        Ok(())
    }

    pub fn write_bytes(&self, offset: usize, src: &[u8]) -> Result<(), UFrameError> {
        check_range(offset, src.len(), self.len_bytes())?;
        if src.is_empty() {
            return Ok(());
        }
        // SAFETY: same invariant as `read_bytes`, swap roles.
        unsafe {
            let dst = phys::phys_to_virt(self.head_paddr).add(offset);
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
        }
        Ok(())
    }

    pub fn read_pod<T: Pod>(&self, offset: usize) -> Result<T, UFrameError> {
        check_range(offset, size_of::<T>(), self.len_bytes())?;
        check_alignment::<T>(self.head_paddr, offset)?;
        // SAFETY: as above; `T: Pod` — every byte pattern is valid.
        let val = unsafe {
            let src = phys::phys_to_virt(self.head_paddr).add(offset) as *const T;
            core::ptr::read(src)
        };
        Ok(val)
    }

    pub fn write_pod<T: Pod>(&self, offset: usize, value: T) -> Result<(), UFrameError> {
        check_range(offset, size_of::<T>(), self.len_bytes())?;
        check_alignment::<T>(self.head_paddr, offset)?;
        // SAFETY: same invariant.
        unsafe {
            let dst = phys::phys_to_virt(self.head_paddr).add(offset) as *mut T;
            core::ptr::write(dst, value);
        }
        Ok(())
    }
}

// `KVec<Frame<M>>` already drops every element when the segment
// drops — Frame<M>::Drop releases the slot. No explicit Drop impl
// needed.

// ---------------------------------------------------------------------------
// UIoSlice / UIoSliceMut: vectored-I/O descriptors.
// ---------------------------------------------------------------------------

/// Read-only vectored-I/O descriptor: `(paddr, len_bytes)`. A
/// descriptor, not a reference — `UIoSlice` deliberately does not
/// expose `&[u8]`. Named to avoid confusion with `std::io::IoSlice`.
#[derive(Clone, Copy, Debug)]
pub struct UIoSlice {
    pub paddr: Paddr,
    pub len: usize,
}

/// Mutable vectored-I/O descriptor: `(paddr, len_bytes)`. Distinct
/// type from [`UIoSlice`] so the type system tracks read-vs-write
/// intent. Neither variant exposes a Rust reference.
#[derive(Debug)]
pub struct UIoSliceMut {
    pub paddr: Paddr,
    pub len: usize,
}

// ---------------------------------------------------------------------------
// Tests (host-side, pure logic).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_range_accepts_full_page() {
        assert!(check_range(0, 4096, 4096).is_ok());
    }

    #[test]
    fn check_range_rejects_overrun() {
        assert_eq!(check_range(4090, 16, 4096), Err(UFrameError::OutOfBounds));
    }

    #[test]
    fn check_range_rejects_arithmetic_overflow() {
        assert_eq!(
            check_range(usize::MAX, 1, 4096),
            Err(UFrameError::OutOfBounds)
        );
    }

    #[test]
    fn check_alignment_accepts_aligned() {
        let p = Paddr::new(0x1000);
        assert!(check_alignment::<u64>(p, 8).is_ok());
    }

    #[test]
    fn check_alignment_rejects_misaligned() {
        let p = Paddr::new(0x1000);
        assert_eq!(check_alignment::<u64>(p, 1), Err(UFrameError::Misaligned));
    }

    #[test]
    fn uframe_is_pointer_sized() {
        // Newtype must be zero-cost: same size as the inner Frame
        // (which is itself a thin pointer to a MetaSlot).
        assert_eq!(
            core::mem::size_of::<UFrame<AnonymousMeta>>(),
            core::mem::size_of::<*const ()>()
        );
    }

    #[test]
    fn uframe_error_is_eq() {
        let a = UFrameError::OutOfBounds;
        let b = UFrameError::OutOfBounds;
        assert_eq!(a, b);
        assert_ne!(a, UFrameError::Misaligned);
    }
}
