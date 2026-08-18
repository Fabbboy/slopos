//! Write-Combining backing allocation for the buffers the display engine scans.
//!
//! The display engine reads scanout surfaces directly from RAM through the GGTT,
//! whose PTEs request no cache snoop, so pixels drawn through the WriteBack HHDM
//! alias would sit in cache while the display scanned stale RAM.

use slopos_abi::PhysAddr;
use slopos_mm::page_alloc::{alloc_kernel_pages, free_page_frame};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_ostd::cpu::x86_64::wbinvd;
use slopos_ostd::mm::io_mem::{IoMemCachePolicy, IoMemRegistry, PhysRange, register_io_mem_range};

/// Allocate `pages` of kernel RAM and return `(phys, wc_virt)`, a
/// Write-Combining CPU alias of it, or `None` (freeing the backing) on failure.
///
/// A one-shot `wbinvd` evicts WriteBack lines the allocator dirtied for these
/// frames, so a later eviction of the HHDM alias cannot land on top of the WC
/// writes; the HHDM alias is never touched again. The caller owns `phys` and
/// frees it via [`free_page_frame`]; the WC mapping is kernel-lifetime and
/// deliberately never unmapped.
///
/// `register_io_mem_range` is single-writer and expects the boot phase, so every
/// caller must allocate during driver probe / bind on the BSP.
pub(crate) fn alloc_wc_scanout(pages: u32) -> Option<(PhysAddr, u64)> {
    let phys = alloc_kernel_pages(pages);
    if phys.is_null() {
        return None;
    }
    wbinvd();
    let size = pages as usize * PAGE_SIZE_4KB as usize;
    if register_io_mem_range(PhysRange {
        base: phys,
        len: size,
    })
    .is_err()
    {
        free_page_frame(phys);
        return None;
    }
    match IoMemRegistry::reserve(phys, size, IoMemCachePolicy::WriteCombining) {
        Ok(wc) => Some((phys, wc.virt_base())),
        Err(_) => {
            free_page_frame(phys);
            None
        }
    }
}
