//! Pinned user buffer — the sound `pin_user_pages(FOLL_PIN)` analogue that
//! backs SlopRing registered / provided buffers (the io_uring zero-copy path).
//!
//! [`PinnedUserBuffer::pin`] walks a user VA range and takes an owning
//! `AnonymousMeta` ref-count on each backing frame (via
//! [`UFrame::wrap_user_paddr`], which hits `from_in_use` for an already-mapped
//! page). While the handle lives the frames cannot be freed or recycled — even
//! if the owner `munmap`s the range — exactly the UAF guard the SlopRing region
//! mapping uses (the user PTE holds one ref, the pin holds a second). `Drop`
//! releases every ref.
//!
//! Access is **volatile byte-copy only** ([`copy_out`](PinnedUserBuffer::copy_out)
//! / [`copy_in`](PinnedUserBuffer::copy_in)) — never a `&[u8]`/`&mut [u8]` over
//! the pages. The pinned memory stays user-mapped and the owner may write it
//! concurrently, so a non-volatile Rust reference would be instant aliasing UB
//! (SLOPRING § 5.3 / framekernel AD-3). Volatile access makes a concurrent user
//! write a well-defined data race instead. This type composes only **safe**
//! `slopos-ostd` primitives, so `mm` stays `#![forbid(unsafe_code)]` and the
//! feature adds no `unsafe` anywhere.
//!
//! Only **anonymous** user memory is pinnable (the meta the page mapping uses,
//! [`ostd_map_4kb_user`](crate::dual_paging::ostd_map_4kb_user)); file- and
//! memfd-backed pages carry a different frame meta and are rejected with
//! [`PinError::NotAnonymous`], matching `IORING_REGISTER_BUFFERS`.

use slopos_abi::addr::VirtAddr;
use slopos_ostd::KVec;
use slopos_ostd::mm::frame::{AnonymousMeta, Paddr};
use slopos_ostd::mm::uframe::{UFrame, UFrameError};

const PAGE_SIZE: usize = 4096;
const PAGE_MASK: u64 = (PAGE_SIZE as u64) - 1;

/// Per-buffer pin ceiling: 1 GiB, matching `IORING_REGISTER_BUFFERS`. Bounds
/// the per-registration page-table walk and the frame `KVec`.
pub const MAX_PIN_BYTES: usize = 1 << 30;

/// Why a pin attempt failed. Every variant maps to a typed errno at the ring
/// boundary — never UB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinError {
    /// `len == 0`, or `va + len` overflows the address space.
    InvalidRange,
    /// The range exceeds [`MAX_PIN_BYTES`].
    TooLarge,
    /// A page in the range is kernel-half / not user-accessible.
    NotUserAccessible,
    /// A page in the range has no present leaf (demand-zero not faulted in).
    NotPresent,
    /// A page is not anonymous memory (file/memfd/ring-backed) — unpinnable.
    NotAnonymous,
    /// Allocating the per-page frame list failed.
    OutOfMemory,
}

/// One contiguous physical run of a pinned buffer, for DMA descriptor
/// programming (a NIC TX descriptor can point at `paddr` directly).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UIoSlice {
    pub paddr: u64,
    pub len: u32,
}

/// A user VA range pinned for the lifetime of this handle. Accessed only
/// through volatile byte-copy; holds one owning frame ref per backing page.
pub struct PinnedUserBuffer {
    /// Byte offset of the pinned range within its first page.
    base_off: usize,
    /// Pinned byte length.
    len: usize,
    /// One owning ref per backing page, in range order. `Drop` releases all.
    frames: KVec<UFrame<AnonymousMeta>>,
}

impl PinnedUserBuffer {
    /// Pin `[va, va + len)` in process `pid`'s address space. Every page must
    /// be present, user-accessible, and anonymous; the whole range is pinned
    /// atomically (on any per-page failure, the refs taken so far are released
    /// as the partial `frames` vec drops).
    pub fn pin(pid: u32, va: u64, len: usize) -> Result<Self, PinError> {
        if len == 0 {
            return Err(PinError::InvalidRange);
        }
        if len > MAX_PIN_BYTES {
            return Err(PinError::TooLarge);
        }
        va.checked_add(len as u64).ok_or(PinError::InvalidRange)?;

        let vm_space =
            crate::process_vm::process_vm_get_vm_space(pid).ok_or(PinError::NotPresent)?;

        let base_off = (va & PAGE_MASK) as usize;
        let first_page = va & !PAGE_MASK;
        let n_pages = (base_off + len).div_ceil(PAGE_SIZE);

        let mut frames = KVec::with_capacity(n_pages).map_err(|_| PinError::OutOfMemory)?;
        let mut page_va = first_page;
        for _ in 0..n_pages {
            let vaddr = VirtAddr::new(page_va);
            if !crate::dual_paging::ostd_is_user_accessible_4kb(&vm_space, vaddr) {
                return Err(PinError::NotUserAccessible);
            }
            let pa = crate::dual_paging::ostd_virt_to_phys_4kb(&vm_space, vaddr);
            if pa.as_u64() == 0 {
                return Err(PinError::NotPresent);
            }
            // Mask off the in-page byte offset `ostd_virt_to_phys_4kb` folds in;
            // a UFrame owns a whole page-aligned frame.
            let page_pa = pa.as_u64() & !PAGE_MASK;
            let frame = UFrame::<AnonymousMeta>::wrap_user_paddr(Paddr::new(page_pa))
                .map_err(|_| PinError::NotAnonymous)?;
            frames.push(frame).map_err(|_| PinError::OutOfMemory)?;
            page_va = page_va.wrapping_add(PAGE_SIZE as u64);
        }

        Ok(Self {
            base_off,
            len,
            frames,
        })
    }

    /// Pinned byte length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` iff the pin covers zero bytes (never produced by [`pin`], which
    /// rejects `len == 0`; present for lint completeness).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Volatile read of `self[off .. off + dst.len()]` into `dst`, transparently
    /// crossing page boundaries. The volatile read makes a concurrent user write
    /// well-defined; the caller acts only on the returned snapshot.
    pub fn copy_out(&self, off: usize, dst: &mut [u8]) -> Result<(), UFrameError> {
        if off.checked_add(dst.len()).is_none_or(|end| end > self.len) {
            return Err(UFrameError::OutOfBounds);
        }
        let mut abs = self.base_off + off;
        let mut pos = 0usize;
        while pos < dst.len() {
            let page_idx = abs / PAGE_SIZE;
            let page_off = abs % PAGE_SIZE;
            let chunk = core::cmp::min(PAGE_SIZE - page_off, dst.len() - pos);
            self.frames[page_idx].copy_out_volatile(page_off, &mut dst[pos..pos + chunk])?;
            abs += chunk;
            pos += chunk;
        }
        Ok(())
    }

    /// Volatile write of `src` into `self[off .. off + src.len()]`, crossing
    /// page boundaries. The volatile write makes a concurrent user read
    /// well-defined.
    pub fn copy_in(&self, off: usize, src: &[u8]) -> Result<(), UFrameError> {
        if off.checked_add(src.len()).is_none_or(|end| end > self.len) {
            return Err(UFrameError::OutOfBounds);
        }
        let mut abs = self.base_off + off;
        let mut pos = 0usize;
        while pos < src.len() {
            let page_idx = abs / PAGE_SIZE;
            let page_off = abs % PAGE_SIZE;
            let chunk = core::cmp::min(PAGE_SIZE - page_off, src.len() - pos);
            self.frames[page_idx].copy_in_volatile(page_off, &src[pos..pos + chunk])?;
            abs += chunk;
            pos += chunk;
        }
        Ok(())
    }

    /// Test-only: fabricate a pin over freshly-allocated kernel frames (no
    /// process VM required), so the buffer-registry logic can run in a kernel
    /// stest. The volatile `copy_in`/`copy_out` work on any `UFrame`, so the
    /// fabricated pin behaves like a real one for the registry's purposes.
    #[cfg(feature = "test-hooks")]
    pub fn alloc_for_test(len: usize) -> Option<Self> {
        let n_pages = len.div_ceil(PAGE_SIZE).max(1);
        let alloc = slopos_ostd::mm::frame_alloc::current_frame_allocator()?;
        let mut frames = KVec::with_capacity(n_pages).ok()?;
        for _ in 0..n_pages {
            let opts = slopos_ostd::mm::FrameAllocOptions::single().zeroed();
            let pa = alloc.alloc(opts)?;
            let frame = UFrame::<AnonymousMeta>::from_unused(pa, AnonymousMeta::default()).ok()?;
            frames.push(frame).ok()?;
        }
        Some(Self {
            base_off: 0,
            len,
            frames,
        })
    }

    /// Coalesced contiguous `(paddr, len)` runs over the pinned range, for
    /// NIC DMA (a TX descriptor can point at each run directly). The
    /// first/last runs honour `base_off` and the tail partial page.
    pub fn io_slices(&self) -> KVec<UIoSlice> {
        let mut out: KVec<UIoSlice> = KVec::new();
        if self.frames.is_empty() {
            return out;
        }
        let mut remaining = self.len;
        let mut run_start: u64 = 0;
        let mut run_len: u32 = 0;
        let mut next_contig: u64 = 0;
        for (i, frame) in self.frames.iter().enumerate() {
            let page_pa = frame.paddr().as_u64();
            let in_page_off = if i == 0 { self.base_off } else { 0 };
            let avail = PAGE_SIZE - in_page_off;
            let seg = core::cmp::min(avail, remaining);
            let seg_pa = page_pa + in_page_off as u64;
            if run_len != 0 && seg_pa == next_contig {
                run_len += seg as u32;
            } else {
                if run_len != 0 {
                    let _ = out.push(UIoSlice {
                        paddr: run_start,
                        len: run_len,
                    });
                }
                run_start = seg_pa;
                run_len = seg as u32;
            }
            next_contig = seg_pa + seg as u64;
            remaining -= seg;
            if remaining == 0 {
                break;
            }
        }
        if run_len != 0 {
            let _ = out.push(UIoSlice {
                paddr: run_start,
                len: run_len,
            });
        }
        out
    }
}
