//! Typed virtqueue-region wrapper.
//!
//! Wraps a [`Frame<KernelMeta>`] with a strongly-typed descriptor
//! array at offset 0 and a bounds-checked payload byte region
//! immediately after. The descriptor type `T` is provided by the
//! consumer (e.g. virtio-net's `VirtqDesc`); the wrapper itself is
//! layout-agnostic beyond requiring `T: Pod`.
//!
//! No new `unsafe` is added in this module: every access funnels
//! through [`Frame::read_at`], [`Frame::write_at`],
//! [`Frame::read_volatile_at`], [`Frame::write_volatile_at`],
//! [`Frame::slice_at`], or [`Frame::slice_at_mut`], all of which
//! bounds-check internally.

use core::marker::PhantomData;

use crate::mm::Pod;
use crate::mm::frame::{Frame, KernelMeta};

/// A virtqueue's worth of typed descriptors plus a bounds-checked
/// payload region carved out of a [`Frame<KernelMeta>`].
pub struct VirtqueueRegion<T: Pod> {
    frame: Frame<KernelMeta>,
    desc_count: usize,
    payload_offset: usize,
    _t: PhantomData<T>,
}

impl<T: Pod> VirtqueueRegion<T> {
    /// Build a region from a freshly-allocated frame and a
    /// descriptor count. Returns `None` if the descriptor array
    /// would not fit in the frame.
    ///
    /// `desc_count == 0` is permitted — the region degenerates to
    /// a payload-only carry, which the caller may use for raw DMA
    /// transfer pages that share the same allocation discipline.
    pub fn new(frame: Frame<KernelMeta>, desc_count: usize) -> Option<Self> {
        let elem_size = core::mem::size_of::<T>();
        let needed = desc_count.checked_mul(elem_size)?;
        // Bounds-check against the frame's HHDM mapping by attempting
        // a zero-byte slice at `needed`; `slice_at(needed, 0)` returns
        // `None` if `needed` itself is past PAGE_SIZE.
        if needed > 0 {
            frame.slice_at(needed - 1, 1)?;
        }
        Some(Self {
            frame,
            desc_count,
            payload_offset: needed,
            _t: PhantomData,
        })
    }

    /// Number of descriptors the region was sized for.
    #[inline]
    pub fn desc_count(&self) -> usize {
        self.desc_count
    }

    /// Byte offset at which the payload area begins (i.e.
    /// `desc_count * size_of::<T>()`).
    #[inline]
    pub fn payload_offset(&self) -> usize {
        self.payload_offset
    }

    /// Borrow the underlying frame. Useful for callers that need
    /// the physical address (`frame().phys_u64()`) to publish into
    /// device registers.
    #[inline]
    pub fn frame(&self) -> &Frame<KernelMeta> {
        &self.frame
    }

    /// Frame's physical address as a `u64`, convenience for the
    /// common DMA-publish path.
    #[inline]
    pub fn phys_u64(&self) -> u64 {
        self.frame.phys_u64()
    }

    /// Copy out the descriptor at `idx` (non-volatile load).
    pub fn desc(&self, idx: usize) -> Option<T> {
        if idx >= self.desc_count {
            return None;
        }
        let off = idx.checked_mul(core::mem::size_of::<T>())?;
        self.frame.read_at::<T>(off)
    }

    /// Volatile load — for descriptors the hardware may concurrently
    /// mutate (e.g. virtio used-ring entries).
    pub fn read_desc_volatile(&self, idx: usize) -> Option<T> {
        if idx >= self.desc_count {
            return None;
        }
        let off = idx.checked_mul(core::mem::size_of::<T>())?;
        self.frame.read_volatile_at::<T>(off)
    }

    /// Copy `value` into the descriptor at `idx`. Returns `false`
    /// if `idx` is out of range.
    pub fn write_desc(&mut self, idx: usize, value: &T) -> bool {
        if idx >= self.desc_count {
            return false;
        }
        let Some(off) = idx.checked_mul(core::mem::size_of::<T>()) else {
            return false;
        };
        self.frame.write_at::<T>(off, value)
    }

    /// Volatile sibling of [`Self::write_desc`] — required for
    /// publishing entries the hardware may observe before the
    /// next memory barrier.
    pub fn write_desc_volatile(&mut self, idx: usize, value: T) -> bool {
        if idx >= self.desc_count {
            return false;
        }
        let Some(off) = idx.checked_mul(core::mem::size_of::<T>()) else {
            return false;
        };
        self.frame.write_volatile_at::<T>(off, value)
    }

    /// Borrow `len` bytes from the payload region starting at
    /// `payload_offset() + offset`. Returns `None` on overflow or
    /// if the slice would extend past the frame.
    pub fn slice_payload(&self, offset: usize, len: usize) -> Option<&[u8]> {
        let abs = self.payload_offset.checked_add(offset)?;
        self.frame.slice_at(abs, len)
    }

    /// Mutable variant of [`Self::slice_payload`].
    pub fn slice_payload_mut(&mut self, offset: usize, len: usize) -> Option<&mut [u8]> {
        let abs = self.payload_offset.checked_add(offset)?;
        self.frame.slice_at_mut(abs, len)
    }

    /// Bytes from the payload region copied into `dst`.
    /// Convenience for callers that don't want the borrow form.
    pub fn read_payload(&self, offset: usize, dst: &mut [u8]) -> bool {
        let Some(abs) = self.payload_offset.checked_add(offset) else {
            return false;
        };
        self.frame.read_slice(abs, dst)
    }

    /// Copy `src` into the payload region starting at
    /// `payload_offset() + offset`.
    pub fn write_payload(&mut self, offset: usize, src: &[u8]) -> bool {
        let Some(abs) = self.payload_offset.checked_add(offset) else {
            return false;
        };
        self.frame.write_slice(abs, src)
    }

    /// Consume the region and return the underlying frame.
    pub fn into_frame(self) -> Frame<KernelMeta> {
        self.frame
    }
}
