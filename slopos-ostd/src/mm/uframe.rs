//! Untyped frames and segments — byte-copy-only physical memory.
//!
//! `UFrame<M>` and `USegment<M>` expose physical memory through a byte-copy
//! interface so user, peripheral and DMA-tampered memory can never be observed
//! as a Rust reference. They deliberately implement neither `Deref` / `AsRef` /
//! `Index` nor expose `as_slice`; the `compile_fail` doctests on [`UFrame`]
//! lock that in, and any addition returning `&T` / `&[u8]` from these types is
//! a soundness regression.

use core::marker::PhantomData;
use core::mem::{align_of, size_of};

use crate::{AllocError, KVec};

use crate::mm::frame::{
    AnonymousMeta, AnyFrameMeta, Frame, FrameError, Paddr, PageTableMeta as _PageTableMeta,
    RingMeta,
};
use crate::mm::phys;
use crate::mm::pod::Pod;
use crate::process::AccountId;
use crate::process::quota::{Charge, try_charge};
use slopos_abi::quota::PinnedBytesAxis;

const PAGE_SIZE: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UFrameError {
    /// `offset + len` exceeds the region's byte length.
    OutOfBounds,
    /// `(paddr + offset) % align_of::<T>() != 0` for a `read_pod` /
    /// `write_pod` call.
    Misaligned,
    /// A vectored byte-copy completed only part of the requested transfer.
    /// Reserved for future vectored-I/O paths; unused today.
    Truncated,
    /// The kernel heap refused `USegment`'s per-segment bookkeeping.
    OutOfMemory,
}

impl From<AllocError> for UFrameError {
    fn from(_: AllocError) -> Self {
        Self::OutOfMemory
    }
}

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
// pages — exactly the case `AnyUFrameMeta` describes.
unsafe impl AnyUFrameMeta for AnonymousMeta {}

// SAFETY: `RingMeta` backs a SlopRing SQ/CQ region mapped read+write into
// userspace and mutated concurrently by a user thread. The kernel reaches the
// bytes only through this module's volatile byte-copy interface, never forming
// a `&Sqe` / `&mut Cqe` over the frame (SLOPRING § 5.2 / § 5.3).
unsafe impl AnyUFrameMeta for RingMeta {}

// Keeps the unused-import lint quiet on `_PageTableMeta`, which is
// deliberately *not* `AnyUFrameMeta`.
const _SENSITIVE: PhantomData<_PageTableMeta> = PhantomData;

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

/// A single physical 4 KiB frame whose contents are untyped — i.e.
/// reachable only through the byte-copy interface defined on this
/// type. Holds one ref to the underlying [`Frame<M>`] for as long as
/// it lives. See module docs for the no-reference discipline.
///
/// Each of the following must fail to compile; if one ever starts passing, a
/// soundness invariant has been broken.
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

/// Paddr and ref-count, never frame contents: a formatter that printed those
/// would be the no-reference rule broken from inside this module.
impl<M: AnyUFrameMeta> core::fmt::Debug for UFrame<M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("UFrame").field(&self.0).finish()
    }
}

impl UFrame<AnonymousMeta> {
    /// Claim a freshly-allocated 4 KiB user paddr: slot UNUSED → TYPED, ref
    /// count 1. `Drop` of the last wrapper returns the page to the registered
    /// frame allocator.
    ///
    /// [`FrameError::StateMismatch`] means the slot was already live, i.e. the
    /// caller does not in fact own this page. That is a refusal rather than an
    /// alias because the two differ in who frees the page, and a caller that
    /// asked to claim has an allocation it must undo.
    pub fn claim_user_paddr(paddr: Paddr) -> Result<Self, FrameError> {
        Ok(Self(Frame::<AnonymousMeta>::from_unused(
            paddr,
            AnonymousMeta::default(),
        )?))
    }

    /// Take an additional ref on a 4 KiB user page that is **already live** —
    /// fork's child mapping the parent's pages, or a second mapping of a memfd
    /// page. The page is freed when the last of them drops, so an alias
    /// outliving its origin cannot dangle.
    ///
    /// [`FrameError::StateMismatch`] means the slot is UNUSED or BUSY: nobody
    /// owns the page, so there is no ref to share.
    pub fn alias_user_paddr(paddr: Paddr) -> Result<Self, FrameError> {
        Ok(Self(Frame::<AnonymousMeta>::from_in_use(paddr)?))
    }
}

impl UFrame<RingMeta> {
    /// Allocate a single zeroed physical frame from the registered frame
    /// allocator and wrap it as an untyped `UFrame<RingMeta>` — the SlopRing
    /// allocation entry point (SLOPRING § 5.1). `None` if no allocator is
    /// registered or the buddy is exhausted.
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

    pub fn paddr(&self) -> Paddr {
        self.0.paddr()
    }

    pub fn reference_count(&self) -> u32 {
        self.0.reference_count()
    }

    /// Drop the untyped wrapper and yield the inner [`Frame<M>`]. Crate-private
    /// to keep the exit door inside the crate: a public one would expose any
    /// future `Frame<M>` helper returning `&[u8]` over frame contents.
    #[allow(dead_code)]
    pub(crate) fn into_frame(self) -> Frame<M> {
        self.0
    }

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
        // SAFETY: `phys_to_virt(self.paddr())` points into a frame this
        // `UFrame` owns (ref-count > 0 for the life of `&self`); the range
        // check above keeps `[src, src + dst.len())` inside the frame; `dst`
        // is a `&mut [u8]`, which cannot alias the frame's kernel-virt mapping.
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

    /// Volatile + acquire load of a `u32` at `offset`. Reads a user-written
    /// index (`sq_tail` / `cq_head`) from a page shared read+write with a
    /// running userspace thread, where the plain byte-copy methods above would
    /// be a data race and `core::ptr::read` would let the body reads be
    /// reordered ahead of the index read. Requires 4-byte alignment.
    pub fn load_u32_acquire(&self, offset: usize) -> Result<u32, UFrameError> {
        check_range(offset, size_of::<u32>(), PAGE_SIZE)?;
        check_alignment::<u32>(self.paddr(), offset)?;
        // SAFETY (Inv. 4/5): the pointer is into a frame this `UFrame` owns
        // (ref-count > 0 for the life of `&self`); the checks above make the
        // read in-bounds and naturally aligned. `read_volatile` makes a
        // concurrent user write well-defined and un-elidable; the following
        // `Acquire` fence orders this load before any later body read, matching
        // the ring's release-store-before-publish ABI. No `&T` is ever formed
        // over the untyped ring frame.
        let val = unsafe {
            let src = phys::phys_to_virt(self.paddr()).add(offset) as *const u32;
            core::ptr::read_volatile(src)
        };
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        Ok(val)
    }

    /// Release + volatile store of a `u32` at `offset`. Publishes a
    /// kernel-owned index (`sq_head` / `cq_tail`) into the shared page after
    /// the corresponding body writes. Requires 4-byte alignment.
    pub fn store_u32_release(&self, offset: usize, value: u32) -> Result<(), UFrameError> {
        check_range(offset, size_of::<u32>(), PAGE_SIZE)?;
        check_alignment::<u32>(self.paddr(), offset)?;
        // SAFETY (Inv. 4/5): same ownership + bounds + alignment argument as
        // `load_u32_acquire`. The `Release` fence orders every preceding body
        // write before this index publication, matching the acquire load
        // userspace reads it with; `write_volatile` makes the store
        // well-defined against a concurrent user reader. No `&mut T` is ever
        // formed over the untyped ring frame.
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
        // SAFETY (Inv. 4/5): `src` points into a frame this `UFrame` owns and
        // the range check above bounds the copy. Each byte is read with
        // `read_volatile`, so a concurrent user write is a well-defined
        // volatile race rather than UB. `dst` is a `&mut [u8]` that cannot
        // alias the frame mapping. No `&T` over the frame.
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
        // `write_volatile`, so a concurrent user read is well-defined. `src`
        // cannot alias the frame mapping. No `&mut T` over the frame.
        unsafe {
            let base = phys::phys_to_virt(self.paddr()).add(offset);
            for (i, b) in src.iter().enumerate() {
                core::ptr::write_volatile(base.add(i), *b);
            }
        }
        Ok(())
    }
}

/// A contiguous run of [`UFrame<M>`]s totalling `len_pages * 4096`
/// bytes. Internally holds one ref per frame in a `KVec`; on Drop,
/// each frame's ref is released, returning the run to the
/// allocator.
///
/// The byte-copy methods address the run as a single byte buffer from offset
/// `0` to `len_bytes()`. Crossing a 4 KiB boundary inside a single `read_bytes`
/// is fine because the run is physically contiguous and the kernel-virt window
/// [`crate::mm::phys::phys_to_virt`] establishes is HHDM-style linear.
pub struct USegment<M: AnyUFrameMeta = AnonymousMeta> {
    /// One [`Frame<M>`] ref per page, reached only through `Drop` — hence
    /// `#[allow(dead_code)]`.
    #[allow(dead_code)]
    frames: KVec<Frame<M>>,
    head_paddr: Paddr,
    len_pages: usize,
    _marker: PhantomData<M>,
}

impl<M: AnyUFrameMeta> USegment<M> {
    /// Build a segment from a run of `len_pages` consecutive unused frames
    /// starting at `head`, installing `M::default()` as metadata for each. On
    /// any per-frame failure the frames already installed are dropped,
    /// releasing their slots.
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
                    // Dropping `frames` releases every slot installed so far.
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

    pub fn head_paddr(&self) -> Paddr {
        self.head_paddr
    }

    pub fn len_pages(&self) -> usize {
        self.len_pages
    }

    pub fn len_bytes(&self) -> usize {
        self.len_pages * PAGE_SIZE
    }

    /// Read-only vectored-I/O descriptor for the run. Always one slice
    /// (segments are physically contiguous); the array shape is
    /// future-proofing for scatter/gather lists.
    pub fn io_slices(&self) -> [UIoSlice; 1] {
        [UIoSlice {
            paddr: self.head_paddr,
            len: self.len_bytes(),
        }]
    }

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
        // SAFETY: the run is physically contiguous and the HHDM window linear,
        // so `phys_to_virt(head) + offset` addresses it; the range check above
        // bounds the copy; `dst` cannot alias the frame mapping; `frames`
        // retains a live ref per page for the lifetime of `&self`.
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

/// Read-only vectored-I/O descriptor: `(paddr, len_bytes)`. A descriptor, not
/// a reference — it deliberately exposes no `&[u8]`.
#[derive(Clone, Copy, Debug)]
pub struct UIoSlice {
    pub paddr: Paddr,
    pub len: usize,
}

/// Mutable vectored-I/O descriptor: `(paddr, len_bytes)`. Distinct type from
/// [`UIoSlice`] so the type system tracks read-vs-write intent.
#[derive(Debug)]
pub struct UIoSliceMut {
    pub paddr: Paddr,
    pub len: usize,
}

/// Coalesce a page chain into contiguous `(paddr, len)` DMA runs for the byte
/// window `[byte_start, byte_start + len)`, where `byte_start` is an absolute
/// offset into the chain (a pinned buffer's `base_off + intra_offset`).
///
/// Walks **only** `len` bytes, never the whole chain, so a partial send never
/// points the NIC at stale tail bytes; physically adjacent pages fold into one
/// run. Stops cleanly if the window runs past the chain.
pub fn coalesce_io_runs<M: AnyUFrameMeta>(
    frames: &[UFrame<M>],
    byte_start: usize,
    len: usize,
) -> KVec<(u64, u32)> {
    let mut out: KVec<(u64, u32)> = KVec::new();
    if len == 0 || frames.is_empty() {
        return out;
    }
    let mut remaining = len;
    let mut page_idx = byte_start / PAGE_SIZE;
    let mut in_page_off = byte_start % PAGE_SIZE;
    let mut run_start: u64 = 0;
    let mut run_len: u32 = 0;
    let mut next_contig: u64 = 0;
    while remaining > 0 && page_idx < frames.len() {
        let page_pa = frames[page_idx].paddr().as_u64();
        let avail = PAGE_SIZE - in_page_off;
        let seg = core::cmp::min(avail, remaining);
        let seg_pa = page_pa + in_page_off as u64;
        if run_len != 0 && seg_pa == next_contig {
            run_len += seg as u32;
        } else {
            if run_len != 0 {
                let _ = out.push((run_start, run_len));
            }
            run_start = seg_pa;
            run_len = seg as u32;
        }
        next_contig = seg_pa + seg as u64;
        remaining -= seg;
        page_idx += 1;
        in_page_off = 0;
    }
    if run_len != 0 {
        let _ = out.push((run_start, run_len));
    }
    out
}

/// Take an **independent** owning ref on every frame in `frames` (re-wrapping
/// each paddr bumps the frame-slot ref count), so the clone keeps the pages
/// pinned even if the original `frames` are dropped. `None` if the per-page
/// list cannot be allocated.
pub fn redup_frames(frames: &[UFrame<AnonymousMeta>]) -> Option<KVec<UFrame<AnonymousMeta>>> {
    let mut out = KVec::with_capacity(frames.len()).ok()?;
    for f in frames.iter() {
        // `f` is a live ref on each page, so this is an alias by construction.
        out.push(UFrame::<AnonymousMeta>::alias_user_paddr(f.paddr()).ok()?)
            .ok()?;
    }
    Some(out)
}

/// Pinned pages held across a DMA that outlives whatever registered them, with
/// the pin charge that accounts for them.
///
/// The charge is independent of the `PinnedUserBuffer` the frames came from and
/// travels with them, refunded by this struct's own `Drop`. Sharing the
/// buffer's `PinnedBytes` charge would refund at ring teardown while the driver
/// still held the pages — a memory-lock bypass at the DMA boundary.
///
/// A retransmit takes a *second* keepalive via [`redup`](Self::redup) and pays
/// for it: counting two in-flight DMAs of the same pages once would let a
/// retransmit storm hold arbitrarily many pages against one charge.
pub struct KeepaliveFrames {
    frames: KVec<UFrame<AnonymousMeta>>,
    charge: Charge<PinnedBytesAxis>,
}

impl KeepaliveFrames {
    /// Take an independent owning ref on every page of `frames`, charged to
    /// `account`. `None` on allocation failure or a refused charge: no
    /// keepalive was taken, so the caller must fall back to the copy path
    /// rather than proceed with pages nothing is holding.
    pub fn take(frames: &[UFrame<AnonymousMeta>], account: AccountId) -> Option<Self> {
        let pages = u32::try_from(frames.len()).ok()?;
        let charge = Charge::commit(try_charge::<PinnedBytesAxis>(account, pages).ok()?);
        Some(Self {
            frames: redup_frames(frames)?,
            charge,
        })
    }

    /// A second, independently charged keepalive over the same pages — one per
    /// in-flight DMA, each released on its own reclaim.
    ///
    /// Charged to the same account, read back off the charge rather than
    /// passed in: a retransmit must not re-home the pin onto whichever
    /// principal happens to be running the send path.
    pub fn redup(&self) -> Option<Self> {
        Self::take(self.frames.as_slice(), self.charge.account())
    }

    #[inline]
    pub fn as_slice(&self) -> &[UFrame<AnonymousMeta>] {
        self.frames.as_slice()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.frames.len()
    }
}

/// Volatile byte-copy of `[byte_start, byte_start + dst.len())` out of a page
/// chain into `dst`, transparently crossing page boundaries. `byte_start` is an
/// absolute offset into the chain. The volatile reads make a concurrent user
/// write well-defined; the caller acts only on the snapshot. `OutOfBounds` if
/// the window runs past the chain.
pub fn copy_out_frames<M: AnyUFrameMeta>(
    frames: &[UFrame<M>],
    byte_start: usize,
    dst: &mut [u8],
) -> Result<(), UFrameError> {
    let mut abs = byte_start;
    let mut pos = 0usize;
    while pos < dst.len() {
        let page_idx = abs / PAGE_SIZE;
        if page_idx >= frames.len() {
            return Err(UFrameError::OutOfBounds);
        }
        let page_off = abs % PAGE_SIZE;
        let chunk = core::cmp::min(PAGE_SIZE - page_off, dst.len() - pos);
        frames[page_idx].copy_out_volatile(page_off, &mut dst[pos..pos + chunk])?;
        abs += chunk;
        pos += chunk;
    }
    Ok(())
}

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
