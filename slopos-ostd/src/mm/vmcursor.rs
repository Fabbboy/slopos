//! Volatile byte cursors over a chain of untyped frames (SlopRing
//! single-direct-copy — SLOPRING § 5.3 / § 13).
//!
//! [`VmReader`] / [`VmWriter`] walk a borrowed `&[UFrame<AnonymousMeta>]` from an
//! absolute byte offset, copying volatilely between the pinned user pages and a
//! kernel slice.
//!
//! Each per-frame copy is clamped to `min(remaining, PAGE_SIZE - intra_off,
//! buf.len())`, so no copy addresses a range spanning two frames (physically
//! non-contiguous, possibly user-writable) — AD-3. The offset/advance bounds are
//! machine-checked in `verification/proofs/vmcursor.rs`.

use crate::mm::frame::AnonymousMeta;
use crate::mm::uframe::UFrame;

const PAGE_SIZE: usize = 4096;

/// A resumable volatile **reader** over a pinned frame chain. The volatile read
/// makes a concurrent user write of these bytes a well-defined race (never UB);
/// the caller acts only on the snapshot it copied out.
#[derive(Clone)]
pub struct VmReader<'a> {
    frames: &'a [UFrame<AnonymousMeta>],
    frame_idx: usize,
    intra_off: usize,
    remaining: usize,
}

/// A resumable volatile **writer** over a pinned frame chain. Deliberately
/// **not** `Clone` (it may alias a `&mut`-like destination).
pub struct VmWriter<'a> {
    frames: &'a [UFrame<AnonymousMeta>],
    frame_idx: usize,
    intra_off: usize,
    remaining: usize,
}

/// `None` if `abs_start + len` runs past the end of the chain.
#[inline]
fn locate(n_frames: usize, abs_start: usize, len: usize) -> Option<(usize, usize)> {
    let total = n_frames.checked_mul(PAGE_SIZE)?;
    if abs_start.checked_add(len)? > total {
        return None;
    }
    Some((abs_start / PAGE_SIZE, abs_start % PAGE_SIZE))
}

impl<'a> VmReader<'a> {
    /// Build a reader over `len` bytes starting at absolute byte offset
    /// `abs_start`. `None` if the range would run past the end of the chain, so
    /// every later read stays in-bounds.
    pub fn new(frames: &'a [UFrame<AnonymousMeta>], abs_start: usize, len: usize) -> Option<Self> {
        let (frame_idx, intra_off) = locate(frames.len(), abs_start, len)?;
        Some(Self {
            frames,
            frame_idx,
            intra_off,
            remaining: len,
        })
    }

    pub fn remain(&self) -> usize {
        self.remaining
    }

    pub fn has_remain(&self) -> bool {
        self.remaining != 0
    }

    /// Volatile-copy up to `dst.len()` bytes from the pinned pages into `dst`,
    /// advancing the cursor; returns the number of bytes copied (`< dst.len()`
    /// only when the cursor runs dry).
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let mut pos = 0;
        while pos < dst.len() && self.remaining > 0 {
            let page_left = PAGE_SIZE - self.intra_off;
            let chunk = self.remaining.min(page_left).min(dst.len() - pos);
            // `frame_idx < frames.len()` while `remaining > 0`: `new` checked the
            // whole range fits, and `chunk` is clamped to `page_left`.
            if self.frames[self.frame_idx]
                .copy_out_volatile(self.intra_off, &mut dst[pos..pos + chunk])
                .is_err()
            {
                break;
            }
            pos += chunk;
            self.remaining -= chunk;
            self.intra_off += chunk;
            if self.intra_off == PAGE_SIZE {
                self.intra_off = 0;
                self.frame_idx += 1;
            }
        }
        pos
    }
}

impl<'a> VmWriter<'a> {
    /// Build a writer over `len` bytes starting at absolute byte offset
    /// `abs_start`. `None` if the range would run past the end of the chain.
    pub fn new(frames: &'a [UFrame<AnonymousMeta>], abs_start: usize, len: usize) -> Option<Self> {
        let (frame_idx, intra_off) = locate(frames.len(), abs_start, len)?;
        Some(Self {
            frames,
            frame_idx,
            intra_off,
            remaining: len,
        })
    }

    pub fn remain(&self) -> usize {
        self.remaining
    }

    pub fn has_remain(&self) -> bool {
        self.remaining != 0
    }

    /// Volatile-copy up to `src.len()` bytes from `src` into the pinned pages,
    /// advancing the cursor; returns the number of bytes copied (`< src.len()`
    /// only when the cursor runs dry).
    pub fn write(&mut self, src: &[u8]) -> usize {
        let mut pos = 0;
        while pos < src.len() && self.remaining > 0 {
            let page_left = PAGE_SIZE - self.intra_off;
            let chunk = self.remaining.min(page_left).min(src.len() - pos);
            if self.frames[self.frame_idx]
                .copy_in_volatile(self.intra_off, &src[pos..pos + chunk])
                .is_err()
            {
                break;
            }
            pos += chunk;
            self.remaining -= chunk;
            self.intra_off += chunk;
            if self.intra_off == PAGE_SIZE {
                self.intra_off = 0;
                self.frame_idx += 1;
            }
        }
        pos
    }
}
