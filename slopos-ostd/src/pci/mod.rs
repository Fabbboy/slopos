//! PCI / PCIe primitives.
//!
//! [`EcamConfigSpace`] wraps an [`IoMem`] region covering one or more bus
//! segments of a PCIe Enhanced Configuration Access Mechanism (ECAM) region,
//! which lays out the whole bus/device/function/register space linearly:
//!
//! ```text
//!   addr = ECAM_BASE
//!        | ((bus - bus_start) << 20)
//!        | (device              << 15)
//!        | (function            << 12)
//!        | register_offset
//! ```

use crate::mm::Pod;
use crate::mm::io_mem::{IoMem, IoMemError};

/// Bus / Device / Function triplet — the canonical PCI address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Bdf {
    /// `device < 32` and `function < 8`, the field widths the PCI / PCIe specs
    /// allocate.
    #[inline]
    pub const fn new(bus: u8, device: u8, function: u8) -> Option<Self> {
        if device >= 32 || function >= 8 {
            None
        } else {
            Some(Self {
                bus,
                device,
                function,
            })
        }
    }

    /// Within-segment ECAM offset, excluding the segment base. `bus_rel` must
    /// already be relative to the segment's `bus_start`.
    #[inline]
    const fn relative_offset(bus_rel: u8, device: u8, function: u8) -> usize {
        ((bus_rel as usize) << 20) | ((device as usize) << 15) | ((function as usize) << 12)
    }
}

/// Errors returned by [`EcamConfigSpace`] accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcamError {
    /// Bus number outside this segment's `[bus_start, bus_end]`.
    BusOutOfRange,
    /// Register offset plus BDF base falls past this segment's mapped region.
    OffsetOutOfRange,
    Io(IoMemError),
}

impl From<IoMemError> for EcamError {
    fn from(value: IoMemError) -> Self {
        EcamError::Io(value)
    }
}

/// One bus-segment view of a PCIe ECAM region, covering `[bus_start, bus_end]`
/// inclusive at both ends per ACPI MCFG semantics. A platform allocates one per
/// MCFG-described segment.
pub struct EcamConfigSpace {
    region: IoMem,
    bus_start: u8,
    bus_end: u8,
}

impl EcamConfigSpace {
    /// Returns `None` if `bus_end < bus_start` or the region is too small for
    /// the implied bus range — each bus is 1 MiB of ECAM.
    pub fn new(region: IoMem, bus_start: u8, bus_end: u8) -> Option<Self> {
        if bus_end < bus_start {
            return None;
        }
        let buses = (bus_end as usize)
            .checked_sub(bus_start as usize)?
            .checked_add(1)?;
        let needed = buses.checked_mul(1usize << 20)?;
        if region.size() < needed {
            return None;
        }
        Some(Self {
            region,
            bus_start,
            bus_end,
        })
    }

    #[inline]
    pub fn bus_range(&self) -> (u8, u8) {
        (self.bus_start, self.bus_end)
    }

    #[inline]
    pub fn contains(&self, bdf: Bdf) -> bool {
        bdf.bus >= self.bus_start && bdf.bus <= self.bus_end
    }

    fn offset_for(&self, bdf: Bdf, offset: u16) -> Result<usize, EcamError> {
        if !self.contains(bdf) {
            return Err(EcamError::BusOutOfRange);
        }
        let bus_rel = bdf.bus - self.bus_start;
        let bdf_off = Bdf::relative_offset(bus_rel, bdf.device, bdf.function);
        bdf_off
            .checked_add(offset as usize)
            .ok_or(EcamError::OffsetOutOfRange)
    }

    /// Volatile read of a `Pod` register in `bdf`'s 4 KiB config space; `None`
    /// on any error, see [`Self::try_read`] for the cause.
    #[inline]
    pub fn read<T: Pod>(&self, bdf: Bdf, offset: u16) -> Option<T> {
        self.try_read::<T>(bdf, offset).ok()
    }

    /// Volatile write of a `Pod` register in `bdf`'s 4 KiB config space; `None`
    /// on any error, see [`Self::try_write`] for the cause.
    #[inline]
    pub fn write<T: Pod>(&self, bdf: Bdf, offset: u16, value: T) -> Option<()> {
        self.try_write::<T>(bdf, offset, value).ok()
    }

    pub fn try_read<T: Pod>(&self, bdf: Bdf, offset: u16) -> Result<T, EcamError> {
        let abs = self.offset_for(bdf, offset)?;
        self.region.try_read::<T>(abs).map_err(EcamError::from)
    }

    pub fn try_write<T: Pod>(&self, bdf: Bdf, offset: u16, value: T) -> Result<(), EcamError> {
        let abs = self.offset_for(bdf, offset)?;
        self.region
            .try_write::<T>(abs, value)
            .map_err(EcamError::from)
    }

    #[inline]
    pub fn region(&self) -> &IoMem {
        &self.region
    }
}
