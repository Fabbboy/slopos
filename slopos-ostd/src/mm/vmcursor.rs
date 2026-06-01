//! Volatile byte cursors over a chain of untyped frames (SlopRing
//! single-direct-copy — SLOPRING § 5.3 / § 13).
//!
//! [`VmReader`] / [`VmWriter`] walk a borrowed `&[UFrame<AnonymousMeta>]`
//! starting at an absolute byte offset within the chain and spanning a fixed
//! length, doing **volatile** byte copies between the pinned user pages and a
//! kernel `&[u8]` / `&mut [u8]`. They are the resumable, page-crossing analogue
//! of [`UFrame::copy_out_volatile`] / [`UFrame::copy_in_volatile`]: a net leaf
//! can pull from / push to a socket buffer one slice-segment at a time without
//! the caller ever staging the whole payload through a kernel scratch buffer.
//!
//! # No new `unsafe`
//!
//! Every byte copy delegates to the existing **safe** wrappers
//! [`UFrame::copy_out_volatile`] / [`UFrame::copy_in_volatile`] (those own the
//! single `read_volatile` / `write_volatile` `unsafe` block, documented
//! `SAFETY (Inv. 4/5)`). This module is entirely safe, so a kernel crate that
//! holds a cursor (`mm`, `net`, `ring`) keeps `#![forbid(unsafe_code)]`.
//!
//! # No cross-frame slice (AD-3)
//!
//! Each per-frame copy is clamped to `min(remaining, PAGE_SIZE - intra_off,
//! buf.len())`, so a single `copy_*_volatile` call never addresses a byte range
//! spanning two frames (which are physically non-contiguous and may be
//! user-writable). A multi-frame transfer loops, issuing N separate per-frame
//! volatile copies. The offset/advance bounds are machine-checked in
//! `verification/proofs/vmcursor.rs`.

use crate::mm::frame::AnonymousMeta;
use crate::mm::uframe::UFrame;

const PAGE_SIZE: usize = 4096;

/// A resumable volatile **reader** over a pinned frame chain: copies bytes
/// *out* of the pinned pages into a kernel buffer. The volatile read makes a
/// concurrent user write of these bytes a well-defined race (never UB); the
/// caller acts only on the snapshot it copied out. Cheap to `Clone` (read-only
/// view).
#[derive(Clone)]
pub struct VmReader<'a> {
    frames: &'a [UFrame<AnonymousMeta>],
    /// Index of the frame the cursor currently points into.
    frame_idx: usize,
    /// Byte offset within `frames[frame_idx]` (`0 ..= PAGE_SIZE`).
    intra_off: usize,
    /// Bytes still available to read before the logical range is exhausted.
    remaining: usize,
}

/// A resumable volatile **writer** over a pinned frame chain: copies bytes
/// *into* the pinned pages from a kernel buffer. The volatile write makes a
/// concurrent user read well-defined. Deliberately **not** `Clone` (it may
/// alias a `&mut`-like destination).
pub struct VmWriter<'a> {
    frames: &'a [UFrame<AnonymousMeta>],
    frame_idx: usize,
    intra_off: usize,
    remaining: usize,
}

/// Resolve a starting absolute byte offset + length against a frame chain into
/// `(frame_idx, intra_off)`, returning `None` if the range runs past the chain.
#[inline]
fn locate(n_frames: usize, abs_start: usize, len: usize) -> Option<(usize, usize)> {
    let total = n_frames.checked_mul(PAGE_SIZE)?;
    if abs_start.checked_add(len)? > total {
        return None;
    }
    Some((abs_start / PAGE_SIZE, abs_start % PAGE_SIZE))
}

impl<'a> VmReader<'a> {
    /// Build a reader over `frames` covering `len` bytes starting at absolute
    /// byte offset `abs_start` within the chain. `None` if the range would run
    /// past the end of the chain (so every later read stays in-bounds).
    pub fn new(frames: &'a [UFrame<AnonymousMeta>], abs_start: usize, len: usize) -> Option<Self> {
        let (frame_idx, intra_off) = locate(frames.len(), abs_start, len)?;
        Some(Self {
            frames,
            frame_idx,
            intra_off,
            remaining: len,
        })
    }

    /// Bytes still available to read.
    pub fn remain(&self) -> usize {
        self.remaining
    }

    /// `true` iff at least one byte remains.
    pub fn has_remain(&self) -> bool {
        self.remaining != 0
    }

    /// Volatile-copy up to `dst.len()` bytes from the pinned pages into `dst`,
    /// advancing the cursor; returns the number of bytes copied (`< dst.len()`
    /// only when the cursor runs dry). Crosses page boundaries by issuing one
    /// per-frame volatile copy per frame touched — never a cross-frame slice.
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let mut pos = 0;
        while pos < dst.len() && self.remaining > 0 {
            let page_left = PAGE_SIZE - self.intra_off;
            let chunk = self.remaining.min(page_left).min(dst.len() - pos);
            // `frame_idx < frames.len()` holds while `remaining > 0`: `new`
            // guaranteed `remaining` bytes fit from the start, and the chunk is
            // clamped to `page_left`, so `intra_off + chunk <= PAGE_SIZE` and
            // the per-frame `copy_out_volatile` range check always passes.
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
    /// Build a writer over `frames` covering `len` bytes starting at absolute
    /// byte offset `abs_start`. `None` if the range runs past the chain.
    pub fn new(frames: &'a [UFrame<AnonymousMeta>], abs_start: usize, len: usize) -> Option<Self> {
        let (frame_idx, intra_off) = locate(frames.len(), abs_start, len)?;
        Some(Self {
            frames,
            frame_idx,
            intra_off,
            remaining: len,
        })
    }

    /// Bytes still available to write.
    pub fn remain(&self) -> usize {
        self.remaining
    }

    /// `true` iff at least one byte of room remains.
    pub fn has_remain(&self) -> bool {
        self.remaining != 0
    }

    /// Volatile-copy up to `src.len()` bytes from `src` into the pinned pages,
    /// advancing the cursor; returns the number of bytes copied (`< src.len()`
    /// only when the cursor runs dry). Crosses page boundaries via one
    /// per-frame volatile copy each — never a cross-frame slice.
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
