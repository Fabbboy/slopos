//! The ring's shared-memory region: a vector of `UFrame<RingMeta>` frames
//! addressed as one flat byte buffer (SLOPRING § 3, § 5).
//!
//! Every kernel access to ring memory goes through this type. The frames are
//! user-writable concurrently, so it never forms a `&Sqe` / `&mut Cqe` over
//! one — only OSTD's bounded volatile byte-copy and `u32` acquire/release
//! accessors (AD-3 / Inv. 4/5).

use slopos_abi::addr::PhysAddr;
use slopos_ostd::KVec;
use slopos_ostd::mm::frame::{Paddr, RingMeta};
use slopos_ostd::mm::uframe::UFrame;

const PAGE_SIZE: usize = 4096;

/// Owns the `UFrame<RingMeta>` frames backing one ring's SQ/CQ region. The
/// kernel holds one ref per frame for the ring's whole lifetime; the user
/// mapping holds an independent `from_in_use` ref per frame (taken in
/// `mm::process_vm_map_ring`), so a mapping that outlives the ring fd cannot
/// UAF (SLOPRING § 14).
pub struct RingRegion {
    frames: KVec<UFrame<RingMeta>>,
    bytes: usize,
}

/// Why a region access failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionError {
    OutOfBounds,
    /// A `u32` index access was not 4-byte aligned within its frame.
    Misaligned,
    OutOfMemory,
}

impl RingRegion {
    /// Allocate `n_pages` zeroed `UFrame<RingMeta>` frames.
    pub fn alloc(n_pages: usize) -> Result<Self, RegionError> {
        let mut frames: KVec<UFrame<RingMeta>> =
            KVec::with_capacity(n_pages).map_err(|_| RegionError::OutOfMemory)?;
        for _ in 0..n_pages {
            let f = UFrame::<RingMeta>::alloc().ok_or(RegionError::OutOfMemory)?;
            frames.push(f).map_err(|_| RegionError::OutOfMemory)?;
        }
        Ok(Self {
            frames,
            bytes: n_pages * PAGE_SIZE,
        })
    }

    #[allow(dead_code)]
    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    #[allow(dead_code)]
    pub fn page_count(&self) -> usize {
        self.frames.len()
    }

    /// Physical addresses of the backing frames, in region order — consumed by
    /// `process_vm_map_ring` to install the user PTEs.
    pub fn paddrs(&self) -> KVec<PhysAddr> {
        let mut out = KVec::with_capacity(self.frames.len()).expect("ring: paddrs alloc");
        for f in self.frames.iter() {
            let pa: Paddr = f.paddr();
            out.push(PhysAddr::new(pa.as_u64()))
                .expect("ring: paddrs push");
        }
        out
    }

    /// Resolve a flat byte `offset` to `(frame_index, in_frame_offset)`,
    /// enforcing that `[offset, offset+len)` lies wholly within one frame —
    /// `abi::ring::RingLayout` page-aligns the SQE/CQE arrays and keeps the
    /// control block in the first page, so nothing ever straddles.
    fn locate(&self, offset: usize, len: usize) -> Result<(usize, usize), RegionError> {
        let end = offset.checked_add(len).ok_or(RegionError::OutOfBounds)?;
        if end > self.bytes {
            return Err(RegionError::OutOfBounds);
        }
        let frame_idx = offset / PAGE_SIZE;
        let in_frame = offset % PAGE_SIZE;
        if in_frame + len > PAGE_SIZE {
            return Err(RegionError::OutOfBounds);
        }
        Ok((frame_idx, in_frame))
    }

    /// Volatile + acquire load of a `u32` at flat `offset` (a user-written
    /// index).
    pub fn load_u32_acquire(&self, offset: usize) -> Result<u32, RegionError> {
        let (fi, off) = self.locate(offset, 4)?;
        self.frames[fi]
            .load_u32_acquire(off)
            .map_err(|_| RegionError::Misaligned)
    }

    /// Release + volatile store of a `u32` at flat `offset` (a kernel-owned
    /// index).
    pub fn store_u32_release(&self, offset: usize, value: u32) -> Result<(), RegionError> {
        let (fi, off) = self.locate(offset, 4)?;
        self.frames[fi]
            .store_u32_release(off, value)
            .map_err(|_| RegionError::Misaligned)
    }

    /// Volatile byte-copy *out* of the region into a private kernel
    /// buffer (the SQE snapshot path — SLOPRING § 5.3).
    pub fn copy_out(&self, offset: usize, dst: &mut [u8]) -> Result<(), RegionError> {
        let (fi, off) = self.locate(offset, dst.len())?;
        self.frames[fi]
            .copy_out_volatile(off, dst)
            .map_err(|_| RegionError::OutOfBounds)
    }

    /// Volatile byte-copy *in* from a private kernel buffer into the
    /// region (the CQE post path).
    pub fn copy_in(&self, offset: usize, src: &[u8]) -> Result<(), RegionError> {
        let (fi, off) = self.locate(offset, src.len())?;
        self.frames[fi]
            .copy_in_volatile(off, src)
            .map_err(|_| RegionError::OutOfBounds)
    }
}
