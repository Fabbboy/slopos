//! PCI / PCIe primitives.
//!
//! Currently exposes [`EcamConfigSpace`] — a typed wrapper over an
//! [`IoMem`] region that covers one or more bus segments of a PCIe
//! Enhanced Configuration Access Mechanism (ECAM) region.
//!
//! ECAM lays out the entire bus/device/function/register address
//! space linearly:
//!
//! ```text
//!   addr = ECAM_BASE
//!        | ((bus - bus_start) << 20)
//!        | (device              << 15)
//!        | (function            << 12)
//!        | register_offset
//! ```
//!
//! `EcamConfigSpace` encodes that arithmetic, bounds-checks the
//! BDF coordinates, and funnels the actual MMIO access through
//! [`IoMem::try_read`] / [`IoMem::try_write`]. No new `unsafe` is
//! introduced beyond the unsafety already absorbed by `IoMem`.

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
    /// Builds a `Bdf` only when `device < 32` and `function < 8`,
    /// matching the field widths the PCI / PCIe specs allocate.
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

    /// Within-segment ECAM offset, *not* including the segment base.
    /// `bus` is treated as already relative to the segment's
    /// `bus_start` — callers should subtract before invoking.
    #[inline]
    const fn relative_offset(bus_rel: u8, device: u8, function: u8) -> usize {
        ((bus_rel as usize) << 20) | ((device as usize) << 15) | ((function as usize) << 12)
    }
}

/// Errors returned by [`EcamConfigSpace`] accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcamError {
    /// The BDF was not malformed but its bus number was outside the
    /// `[bus_start, bus_end]` interval covered by this segment.
    BusOutOfRange,
    /// The PCI / PCIe register offset added to the BDF base would
    /// fall past the end of this segment's mapped region.
    OffsetOutOfRange,
    /// Underlying MMIO error from `IoMem`.
    Io(IoMemError),
}

impl From<IoMemError> for EcamError {
    fn from(value: IoMemError) -> Self {
        EcamError::Io(value)
    }
}

/// One bus-segment view of a PCIe ECAM region.
///
/// Each `EcamConfigSpace` covers `[bus_start, bus_end]` (inclusive
/// at both ends, matching ACPI MCFG semantics). A platform with
/// multiple PCI segments allocates one `EcamConfigSpace` per
/// MCFG-described segment.
pub struct EcamConfigSpace {
    region: IoMem,
    bus_start: u8,
    bus_end: u8,
}

impl EcamConfigSpace {
    /// Build a config-space view. Returns `None` if `bus_end <
    /// bus_start` or if the region is too small to host the implied
    /// bus range (each bus is 1 MiB == `1 << 20` bytes of ECAM).
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

    /// `[bus_start, bus_end]` inclusive bounds of this segment.
    #[inline]
    pub fn bus_range(&self) -> (u8, u8) {
        (self.bus_start, self.bus_end)
    }

    /// `true` when `bdf.bus` falls within this segment's bus range.
    #[inline]
    pub fn contains(&self, bdf: Bdf) -> bool {
        bdf.bus >= self.bus_start && bdf.bus <= self.bus_end
    }

    /// Compute the absolute byte offset of `bdf:offset` within the
    /// `IoMem` region.
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

    /// Volatile read of a `Pod` register at `offset` inside `bdf`'s
    /// 4 KiB config space. Returns `None` on any error; use
    /// [`Self::try_read`] when the caller wants the specific cause.
    #[inline]
    pub fn read<T: Pod>(&self, bdf: Bdf, offset: u16) -> Option<T> {
        self.try_read::<T>(bdf, offset).ok()
    }

    /// Volatile write of a `Pod` register at `offset` inside `bdf`'s
    /// 4 KiB config space. Returns `None` on any error; use
    /// [`Self::try_write`] for the specific cause.
    #[inline]
    pub fn write<T: Pod>(&self, bdf: Bdf, offset: u16, value: T) -> Option<()> {
        self.try_write::<T>(bdf, offset, value).ok()
    }

    /// Fallible variant of [`Self::read`].
    pub fn try_read<T: Pod>(&self, bdf: Bdf, offset: u16) -> Result<T, EcamError> {
        let abs = self.offset_for(bdf, offset)?;
        self.region.try_read::<T>(abs).map_err(EcamError::from)
    }

    /// Fallible variant of [`Self::write`].
    pub fn try_write<T: Pod>(&self, bdf: Bdf, offset: u16, value: T) -> Result<(), EcamError> {
        let abs = self.offset_for(bdf, offset)?;
        self.region
            .try_write::<T>(abs, value)
            .map_err(EcamError::from)
    }

    /// Borrow the underlying `IoMem` — for callers that need to
    /// pass it back through legacy APIs during the consumer-side
    /// migration.
    #[inline]
    pub fn region(&self) -> &IoMem {
        &self.region
    }
}
