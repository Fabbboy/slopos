//! Bridges `slopos_ostd::mm::io_mem::IoMemMapper` to the kernel-virt
//! MMIO window allocator and the kernel-half mapping path.
//!
//! Lives in `slopos-mm` (rather than `slopos-ostd`) so `slopos-ostd`
//! has no dependency on `slopos-mm`. The boot path registers it with
//! `slopos_ostd::mm::register_io_mem_mapper` before the init phases
//! run, so it backs every `MmioRegion::map` in the kernel.
//!
//! Cache-policy mapping mirrors the firmware-default PAT layout
//! described in [`crate::pat`]: WriteCombining sets PWT=1 (PA1 = WC),
//! Uncacheable sets PCD=1 (PA4 = UC), WriteThrough also sets PWT=1
//! against the unmodified PAT default (PA1 = WT), WriteBack leaves
//! both bits clear (PA0 = WB).

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::alignment::align_up_u64;
use slopos_ostd::mm::io_mem::{IoMemCachePolicy, IoMemError, IoMemMapper};

use crate::kernel_mappings::kernel_map_io_4kb;
use crate::memory_layout_defs::{MMIO_VIRT_BASE, MMIO_VIRT_SIZE};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};

static MMIO_NEXT_VIRT: AtomicU64 = AtomicU64::new(MMIO_VIRT_BASE);

fn alloc_virt(size: u64) -> Option<u64> {
    let aligned_size = align_up_u64(size, PAGE_SIZE_4KB);
    let mut current = MMIO_NEXT_VIRT.load(Ordering::Relaxed);
    loop {
        let new_next = current.checked_add(aligned_size)?;
        if new_next > MMIO_VIRT_BASE + MMIO_VIRT_SIZE {
            return None;
        }
        match MMIO_NEXT_VIRT.compare_exchange_weak(
            current,
            new_next,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Some(current),
            Err(actual) => current = actual,
        }
    }
}

fn flags_for(policy: IoMemCachePolicy) -> u64 {
    let base =
        PageFlags::PRESENT.bits() | PageFlags::WRITABLE.bits() | PageFlags::NO_EXECUTE.bits();
    match policy {
        IoMemCachePolicy::Uncacheable => base | PageFlags::CACHE_DISABLE.bits(),
        IoMemCachePolicy::WriteCombining => base | PageFlags::WRITE_THROUGH.bits(),
        IoMemCachePolicy::WriteThrough => base | PageFlags::WRITE_THROUGH.bits(),
        IoMemCachePolicy::WriteBack => base,
    }
}

pub struct LegacyIoMemMapperShim;

pub static LEGACY_IO_MEM_MAPPER_SHIM: LegacyIoMemMapperShim = LegacyIoMemMapperShim;

/// Doubly-indirect handle the OSTD `register_io_mem_mapper` hook
/// consumes — `&'static &'static dyn IoMemMapper`. `pub` because the
/// boot caller in `boot::early_init::kernel_main_impl` registers it
/// inline (the former `register_with_ostd(token)` shim has been
/// inlined, taking `&BspToken<'_>` from the boot ctx).
pub static LEGACY_IO_MEM_MAPPER_DYN: &dyn IoMemMapper = &LEGACY_IO_MEM_MAPPER_SHIM;

impl IoMemMapper for LegacyIoMemMapperShim {
    fn map(
        &self,
        phys: PhysAddr,
        size: usize,
        policy: IoMemCachePolicy,
    ) -> Result<u64, IoMemError> {
        if phys.is_null() || size == 0 {
            return Err(IoMemError::MappingFailed);
        }
        let aligned_phys = phys.as_u64() & !(PAGE_SIZE_4KB - 1);
        let offset_in_page = phys.as_u64() - aligned_phys;
        let total = align_up_u64(offset_in_page + size as u64, PAGE_SIZE_4KB);
        let num_pages = total / PAGE_SIZE_4KB;
        let virt_base = alloc_virt(total).ok_or(IoMemError::MappingFailed)?;
        let flags = flags_for(policy);
        for i in 0..num_pages {
            let page_phys = PhysAddr::new(aligned_phys + i * PAGE_SIZE_4KB);
            let page_virt = VirtAddr::new(virt_base + i * PAGE_SIZE_4KB);
            if kernel_map_io_4kb(page_virt, page_phys, flags) != 0 {
                return Err(IoMemError::MappingFailed);
            }
        }
        Ok(virt_base + offset_in_page)
    }

    fn unmap(&self, _virt: u64, _size: usize) {
        // Mappings are leaked: the legacy kernel-virt allocator at
        // `MMIO_NEXT_VIRT` is bump-only and has no recycle path.
    }
}
