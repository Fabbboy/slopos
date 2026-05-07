//! DMA-region primitives.
//!
//! This module hosts safe wrappers over [`Frame<KernelMeta>`]
//! tailored to device-visible memory layouts. The streaming /
//! coherent DMA handles for IOMMU-mapped buffers live at
//! [`crate::mm::dma`] (re-exported at the crate root); the types
//! here describe the *layout discipline* that drivers impose on
//! those frames once they own them.
//!
//! Currently exposed:
//! - [`VirtqueueRegion<T>`] — typed access to a virtio descriptor
//!   ring stored at the start of a frame, with a bounds-checked
//!   payload area immediately after.

pub mod virtqueue;

pub use virtqueue::VirtqueueRegion;
