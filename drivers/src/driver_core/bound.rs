//! Bus-agnostic [`BoundDevice`] vends, plus the PCI-only ones.
//!
//! The struct itself lives in [`crate::driver_core::bus`]; its inherent impls
//! are split so each bus's vends sit beside the bus that needs them. A method
//! name may appear in only one of the three impls — a collision between the
//! generic impl and a bus-specific one is a duplicate definition, not a
//! shadow.

use slopos_abi::PhysAddr;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_ostd::io::port::IoPortError;
use slopos_ostd::irq::{IrqAllocator, IrqContext, IrqError};
use slopos_ostd::mm::{DmaCoherent, DmaDirection, DmaError, DmaStream, IoMem};

use crate::driver_core::bus::{BoundDevice, Bus};
use crate::pci::PciBus;

/// Why a [`BoundDevice`] vend failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundError {
    /// The heap could not box the resource or grow the `Devres` bag.
    OutOfMemory,
    Dma(DmaError),
    Irq(IrqError),
    /// Not on the certified-insensitive list, or already held.
    IoPort(IoPortError),
    /// The BAR index does not exist, is an I/O BAR, or firmware left it unassigned.
    NoSuchBar,
    MapFailed,
}

impl<'d, B: Bus + 'static> BoundDevice<'d, B> {
    /// Escape hatch for resource kinds without a dedicated vend method; `res`
    /// drops on probe failure or unbind.
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
    /// anchor, a clone is a working copy.
    pub fn map_region(&mut self, phys: PhysAddr, len: usize) -> Result<&IoMem, BoundError> {
        let region = MmioRegion::map(phys, len).ok_or(BoundError::MapFailed)?;
        self.res.attach(region).map_err(|_| BoundError::OutOfMemory)
    }

    /// Allocate an IDT vector, install `handler` on it, and hand the owned
    /// binding to the bag. Returns the vector by value, so no borrow lingers.
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

impl<'d> BoundDevice<'d, PciBus> {
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
}
