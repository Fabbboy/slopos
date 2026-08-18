//! `BoundDevice` — the single capability handed to a driver's `probe`.
//!
//! A probe vends MMIO windows, IRQ bindings and DMA buffers through it. Every
//! vend hands ownership to the device's [`Devres`] bag, so a probe that fails
//! partway releases what it took, in reverse order, when the registry drops the
//! bag; on success the bag lives for the binding's lifetime.

use slopos_abi::PhysAddr;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_ostd::dev::Devres;
use slopos_ostd::io::port::IoPortError;
use slopos_ostd::irq::{IrqAllocator, IrqContext, IrqError};
use slopos_ostd::mm::{DmaCoherent, DmaDirection, DmaError, DmaStream, IoMem};

use crate::pci_defs::PciDeviceInfo;

/// Why a [`BoundDevice`] vend failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundError {
    /// The heap could not box the resource or grow the [`Devres`] bag.
    OutOfMemory,
    /// A DMA buffer could not be allocated or mapped.
    Dma(DmaError),
    /// An IRQ vector could not be allocated or its callback installed.
    Irq(IrqError),
    /// An I/O port could not be reserved (not on the certified-insensitive list,
    /// or already held).
    IoPort(IoPortError),
    /// The requested BAR index does not exist, is an I/O (not memory) BAR, or
    /// the firmware left it unassigned.
    NoSuchBar,
    /// The MMIO window could not be reserved or mapped.
    MapFailed,
}

/// The capability a probe drives to acquire device resources.
pub struct BoundDevice<'d> {
    info: &'d PciDeviceInfo,
    res: &'d mut Devres,
}

impl<'d> BoundDevice<'d> {
    pub fn new(info: &'d PciDeviceInfo, res: &'d mut Devres) -> Self {
        Self { info, res }
    }

    /// The device's PCI enumeration record. `PciDeviceInfo` is `Copy`, so a
    /// probe snapshots it (`let info = *bound.info();`) to free the borrow for
    /// subsequent `&mut self` vend calls.
    #[inline]
    pub fn info(&self) -> &PciDeviceInfo {
        self.info
    }

    /// Escape hatch for resource kinds without a dedicated vend method (e.g. an
    /// [`slopos_ostd::irq::OwnedIrq`] already programmed by a bus-specific
    /// helper). `res` drops on probe failure or unbind.
    pub fn attach<T: Send + Sync + 'static>(&mut self, res: T) -> Result<&T, BoundError> {
        self.res.attach(res).map_err(|_| BoundError::OutOfMemory)
    }

    /// Allocate `npages` of IOMMU-mapped, cache-coherent DMA memory.
    pub fn alloc_dma_coherent(&mut self, npages: usize) -> Result<&DmaCoherent, BoundError> {
        let dma = DmaCoherent::alloc(npages).map_err(BoundError::Dma)?;
        self.res.attach(dma).map_err(|_| BoundError::OutOfMemory)
    }

    /// Allocate `npages` of IOMMU-mapped streaming DMA memory in `dir`.
    pub fn alloc_dma_stream(
        &mut self,
        npages: usize,
        dir: DmaDirection,
    ) -> Result<&DmaStream, BoundError> {
        let dma = DmaStream::alloc(npages, dir).map_err(BoundError::Dma)?;
        self.res.attach(dma).map_err(|_| BoundError::OutOfMemory)
    }

    /// Reserve and map `[phys, phys + len)` as an uncacheable MMIO window.
    ///
    /// The returned [`IoMem`] is `Clone`: the bag cell is the leak-tracking
    /// anchor, a clone is a working copy whose VA persists for kernel lifetime.
    pub fn map_region(&mut self, phys: PhysAddr, len: usize) -> Result<&IoMem, BoundError> {
        let region = MmioRegion::map(phys, len).ok_or(BoundError::MapFailed)?;
        self.res.attach(region).map_err(|_| BoundError::OutOfMemory)
    }

    /// Map a length-`len` window of BAR `bar` at byte `offset` within it.
    pub fn map_bar(&mut self, bar: u8, offset: u32, len: usize) -> Result<&IoMem, BoundError> {
        let b = self
            .info
            .bars
            .get(bar as usize)
            .ok_or(BoundError::NoSuchBar)?;
        if b.base == 0 || b.is_io != 0 {
            return Err(BoundError::NoSuchBar);
        }
        let phys = PhysAddr::new(b.base.wrapping_add(offset as u64));
        self.map_region(phys, len)
    }

    /// Allocate an IDT vector, install `handler` on it, and hand the owned
    /// binding to the bag. Returns the vector number by value, so no borrow
    /// lingers and the caller can vend again immediately.
    pub fn request_irq<F>(&mut self, handler: F) -> Result<u8, BoundError>
    where
        F: Fn(&IrqContext<'_>) + Send + Sync + 'static,
    {
        let line = IrqAllocator::alloc().map_err(BoundError::Irq)?;
        let vector = line.vector();
        let owned = line
            .register_callback_owned(handler)
            .map_err(BoundError::Irq)?;
        self.res
            .attach(owned)
            .map_err(|_| BoundError::OutOfMemory)?;
        Ok(vector)
    }
}
