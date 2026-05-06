//! `MmioRegion` — kernel-side alias for `slopos_ostd::mm::io_mem::IoMem`.
//!
//! All call sites go through OSTD's gated `IoMemRegistry`. The
//! [`MmioRegionExt`] trait carries the legacy `map` / `map_page` /
//! `map_1mb` constructors so existing driver code keeps compiling
//! without changing call shape — under the hood, each `map(...)`
//! registers the requested phys range with OSTD's dynamic-range
//! secondary registry and then asks `IoMemRegistry::reserve` for the
//! mapping. The single MMIO virt allocator now lives in
//! [`crate::io_mem_mapper_shim`].

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::io_mem::{
    IoMem, IoMemCachePolicy, IoMemRegistry, PhysRange, register_io_mem_range,
};

use crate::paging_defs::PAGE_SIZE_4KB;

pub type MmioRegion = IoMem;

/// Legacy constructor surface for [`MmioRegion`]. `IoMem` itself is
/// only buildable through `IoMemRegistry::reserve` (or the const
/// [`MmioRegion::empty`] placeholder); this trait re-introduces the
/// `MmioRegion::map(phys, size)` shape that ~14 driver call-sites
/// already use.
pub trait MmioRegionExt: Sized {
    /// Allocate a kernel virtual window for `[phys, phys + size)`,
    /// install Uncacheable page-table entries, and return the
    /// resulting [`MmioRegion`]. Returns `None` for null phys, zero
    /// size, address-space overflow, or registry / mapper failure.
    fn map(phys: PhysAddr, size: usize) -> Option<Self>;

    /// `map(phys, PAGE_SIZE_4KB)`.
    fn map_page(phys: PhysAddr) -> Option<Self>;

    /// `map(phys, 1 MiB)`.
    fn map_1mb(phys: PhysAddr) -> Option<Self>;
}

impl MmioRegionExt for MmioRegion {
    fn map(phys: PhysAddr, size: usize) -> Option<Self> {
        if phys.is_null() || size == 0 {
            return None;
        }
        if phys.as_u64().checked_add(size as u64)? > PhysAddr::MAX.as_u64() {
            return None;
        }
        register_io_mem_range(PhysRange {
            base: phys,
            len: size,
        })
        .ok()?;
        IoMemRegistry::reserve(phys, size, IoMemCachePolicy::Uncacheable).ok()
    }

    fn map_page(phys: PhysAddr) -> Option<Self> {
        Self::map(phys, PAGE_SIZE_4KB as usize)
    }

    fn map_1mb(phys: PhysAddr) -> Option<Self> {
        Self::map(phys, 1024 * 1024)
    }
}
