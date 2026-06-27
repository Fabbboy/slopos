//! Write-Combining backing allocation for the buffers the display engine scans.
//!
//! The Intel display engine reads a scanout surface **directly from RAM** through
//! the GGTT, whose PTEs request no cache snoop (see [`crate::xe_logic::ggtt_pte`]).
//! A buffer drawn through the ordinary WriteBack HHDM alias would leave the pixel
//! writes sitting in the CPU cache while the display scans stale RAM — a moving
//! corruption wave. The firmware/Limine framebuffer avoids this because it is
//! mapped Write-Combining; this helper gives xe's own scanout buffers the same
//! treatment, using the safe `slopos-ostd` I/O-memory mapper (the BAR-mapping
//! machinery), so no `unsafe` enters the `drivers` crate.

use slopos_abi::PhysAddr;
use slopos_mm::page_alloc::{alloc_kernel_pages, free_page_frame};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_ostd::cpu::x86_64::wbinvd;
use slopos_ostd::mm::io_mem::{IoMemCachePolicy, IoMemRegistry, PhysRange, register_io_mem_range};

/// Allocate `pages` of kernel RAM and return its physical base together with a
/// Write-Combining CPU alias of it.
///
/// Pixels written through the returned `wc_virt` bypass the WriteBack cache and
/// reach RAM, so the display engine's direct GGTT read always sees fresh data. A
/// one-shot `wbinvd` first evicts any WriteBack lines the allocator dirtied for
/// these frames, so a later eviction of the HHDM alias cannot land on top of the
/// WC writes (the HHDM alias is never touched again).
///
/// Returns `(phys, wc_virt)`, or `None` (freeing the backing) on failure. The
/// caller owns `phys` — it GGTT-maps it and frees it via [`free_page_frame`] on
/// rollback. The WC mapping is kernel-lifetime and intentionally never unmapped.
///
/// `register_io_mem_range` is single-writer and expects the boot phase, so every
/// caller must allocate during driver probe / bind (on the BSP), never from a
/// later compositor-driven path.
pub(crate) fn alloc_wc_scanout(pages: u32) -> Option<(PhysAddr, u64)> {
    let phys = alloc_kernel_pages(pages);
    if phys.is_null() {
        return None;
    }
    // Flush any WriteBack lines the allocator left dirty (e.g. zero-fill) before
    // the first WC write, so a stray eviction cannot clobber WC pixels in RAM.
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
