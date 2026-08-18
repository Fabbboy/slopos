//! DMA-region primitives: the *layout discipline* drivers impose on frames they
//! already own. The streaming / coherent DMA handles for IOMMU-mapped buffers
//! live at [`crate::mm::dma`] instead.

pub mod virtqueue;

pub use virtqueue::VirtqueueRegion;
