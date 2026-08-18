//! Pinned user buffer — the sound `pin_user_pages(FOLL_PIN)` analogue that
//! backs SlopRing registered / provided buffers.
//!
//! [`PinnedUserBuffer::pin`] takes an owning `AnonymousMeta` ref on each backing
//! frame, so while the handle lives the frames cannot be freed or recycled —
//! even if the owner `munmap`s the range.
//!
//! Access is **volatile byte-copy only**: the pinned memory stays user-mapped
//! and the owner may write it concurrently, so a `&[u8]`/`&mut [u8]` over the
//! pages would be instant aliasing UB (SLOPRING § 5.3 / framekernel AD-3).
//!
//! Only **anonymous** user memory is pinnable; file- and memfd-backed pages
//! carry a different frame meta and are rejected with
//! [`PinError::NotAnonymous`].

use slopos_abi::addr::VirtAddr;
use slopos_abi::quota::PinnedBytesAxis;
use slopos_ostd::KVec;
use slopos_ostd::mm::frame::{AnonymousMeta, Paddr};
use slopos_ostd::mm::uframe::{KeepaliveFrames, UFrame, UFrameError};
use slopos_ostd::mm::vmcursor::{VmReader, VmWriter};
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Charge, try_charge};

const PAGE_SIZE: usize = 4096;
const PAGE_MASK: u64 = (PAGE_SIZE as u64) - 1;

/// Per-buffer pin ceiling: 1 GiB, matching `IORING_REGISTER_BUFFERS`. Bounds
/// the per-registration page-table walk and the frame `KVec`.
pub const MAX_PIN_BYTES: usize = 1 << 30;

/// Why a pin attempt failed; every variant maps to a typed errno at the ring
/// boundary.
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
/// programming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UIoSlice {
    pub paddr: u64,
    pub len: u32,
}

/// A user VA range pinned for the lifetime of this handle.
pub struct PinnedUserBuffer {
    base_off: usize,
    len: usize,
    /// One owning ref per backing page, in range order.
    frames: KVec<UFrame<AnonymousMeta>>,
    /// Charged to the ring owner and refunded by this struct's own `Drop` with
    /// exactly the number it holds.
    #[expect(dead_code, reason = "held for ownership; dropping it is the refund")]
    pin_charge: Charge<PinnedBytesAxis>,
}

impl PinnedUserBuffer {
    /// Pin `[va, va + len)` in `process`'s address space. Every page must be
    /// present, user-accessible and anonymous; the range is all-or-nothing —
    /// on a per-page failure the partial `frames` vec drops the refs taken.
    pub fn pin(
        process: slopos_ostd::process::ProcessId,
        va: u64,
        len: usize,
        account: AccountId,
    ) -> Result<Self, PinError> {
        if len == 0 {
            return Err(PinError::InvalidRange);
        }
        if len > MAX_PIN_BYTES {
            return Err(PinError::TooLarge);
        }
        va.checked_add(len as u64).ok_or(PinError::InvalidRange)?;

        // Charged in **pages**, not bytes: `MAX_PIN_BYTES` does not fit the
        // arena's `u32` amount, and a byte count would let a thousand sub-page
        // pins look cheap while each holds a whole frame.
        let pinned_pages = ((va & PAGE_MASK) as usize + len).div_ceil(PAGE_SIZE);
        // Charged before any frame is pinned, so a refusal costs nothing to unwind.
        let pin_charge = Charge::commit(
            try_charge::<PinnedBytesAxis>(account, pinned_pages as u32)
                .map_err(|_| PinError::OutOfMemory)?,
        );

        let vm_space =
            crate::process_vm::process_vm_get_vm_space(process).ok_or(PinError::NotPresent)?;

        let base_off = (va & PAGE_MASK) as usize;
        let first_page = va & !PAGE_MASK;
        let n_pages = (base_off + len).div_ceil(PAGE_SIZE);

        let mut frames = KVec::with_capacity(n_pages).map_err(|_| PinError::OutOfMemory)?;
        let mut page_va = first_page;
        for _ in 0..n_pages {
            let vaddr = VirtAddr::new(page_va);
            if !crate::user_mappings::ostd_is_user_accessible_4kb(&vm_space, vaddr) {
                return Err(PinError::NotUserAccessible);
            }
            let pa = crate::user_mappings::ostd_virt_to_phys_4kb(&vm_space, vaddr);
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
            pin_charge,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` iff the pin covers zero bytes (never produced by [`pin`], which
    /// rejects `len == 0`; present for lint completeness).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Volatile read of `self[off .. off + dst.len()]` into `dst`, transparently
    /// crossing page boundaries.
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
    /// page boundaries.
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

    /// A volatile [`VmReader`] over `self[off .. off + len]` — the
    /// single-direct-copy send path. `None` if the range is out of bounds.
    pub fn reader(&self, off: usize, len: usize) -> Option<VmReader<'_>> {
        if off.checked_add(len)? > self.len {
            return None;
        }
        VmReader::new(self.frames.as_slice(), self.base_off + off, len)
    }

    /// A volatile [`VmWriter`] over `self[off .. off + len]` — the
    /// single-direct-copy recv path. `None` if the range is out of bounds.
    pub fn writer(&self, off: usize, len: usize) -> Option<VmWriter<'_>> {
        if off.checked_add(len)? > self.len {
            return None;
        }
        VmWriter::new(self.frames.as_slice(), self.base_off + off, len)
    }

    /// Test-only: fabricate a pin over freshly-allocated kernel frames (no
    /// process VM required), so the buffer-registry logic can run in a kernel
    /// stest.
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
            // A fabricated pin belongs to no process; a charge against no
            // account is a vacuous success that debits and refunds nothing.
            pin_charge: Charge::commit(try_charge::<PinnedBytesAxis>(AccountId::NONE, 0).ok()?),
        })
    }

    /// Coalesced contiguous `(paddr, len)` runs over the whole pinned range,
    /// for NIC DMA (a TX descriptor can point at each run directly). The
    /// first/last runs honour `base_off` and the tail partial page.
    pub fn io_slices(&self) -> KVec<UIoSlice> {
        self.io_slices_len(self.len)
    }

    /// In-page byte offset of the pinned range within its first backing page.
    /// The TCP `MSG_ZEROCOPY` send queue pairs it with
    /// [`keepalive_frames`](Self::keepalive_frames) to re-derive a segment's DMA
    /// runs at an arbitrary offset on every (re)transmit.
    pub fn base_off(&self) -> usize {
        self.base_off
    }

    /// Coalesced contiguous `(paddr, len)` runs over the **first `len` bytes**
    /// of the pinned range (`len` clamped to the pin length), for a zero-copy
    /// send of fewer bytes than the registered buffer holds. Walking only the
    /// send length is load-bearing: coalescing the whole pin would point the
    /// NIC at stale tail bytes past the datagram.
    pub fn io_slices_len(&self, len: usize) -> KVec<UIoSlice> {
        let mut out: KVec<UIoSlice> = KVec::new();
        if self.frames.is_empty() {
            return out;
        }
        let mut remaining = len.min(self.len);
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

    /// Coalesced `(paddr, len)` DMA runs for the sub-range `[off, off + len)` of
    /// the pinned buffer — the offset-aware form the TCP `MSG_ZEROCOPY` send queue
    /// needs to DMA a segment from the **middle** of a zero-copy send (MSS
    /// segmentation + selective retransmit at arbitrary offsets). Returns empty
    /// if the range runs past the pin.
    pub fn io_runs_at(&self, off: usize, len: usize) -> KVec<(u64, u32)> {
        if off.checked_add(len).is_none_or(|end| end > self.len) {
            return KVec::new();
        }
        slopos_ostd::mm::uframe::coalesce_io_runs(self.frames.as_slice(), self.base_off + off, len)
    }

    /// Take an **independent** owning ref on every backing page, so the pages
    /// stay pinned even if this `PinnedUserBuffer` (and the registry that owns
    /// it) is dropped. A NIC TX DMA driven from [`io_slices`](Self::io_slices)
    /// outlives the ring on a process exit / ring-fd close; the driver holds
    /// this keepalive in its TX slot and drops it only after the device
    /// reclaims the descriptor, closing the use-after-free where the pages
    /// would be recycled mid-DMA. `None` if the per-page list cannot be
    /// allocated.
    ///
    /// # Accounting
    ///
    /// These frames are **not** covered by this pin's `PinnedBytes` charge,
    /// and deliberately so: the keepalive outlives the ring — that is its
    /// whole purpose — so a shared charge would be refunded when the ring went
    /// away while the driver still held the pages, which is a memory-lock
    /// bypass at exactly the DMA boundary. [`KeepaliveFrames`] carries its own
    /// independent charge instead.
    pub fn keepalive_frames(&self, account: AccountId) -> Option<KeepaliveFrames> {
        KeepaliveFrames::take(self.frames.as_slice(), account)
    }
}
