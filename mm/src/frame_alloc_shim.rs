//! Bridges `slopos_ostd::mm::frame::FrameAlloc` to the legacy buddy
//! allocator in [`crate::page_alloc`].
//!
//! Lives in `slopos-mm` (rather than `slopos-ostd`) so `slopos-ostd`
//! has no dependency on `slopos-mm`. Multi-page allocations route to
//! the buddy's `alloc_page_frames` path; the buddy stores the
//! allocation order in the frame descriptor, so `dealloc` recovers
//! it from `paddr` alone and `size_pages` is only validated.

use slopos_ostd::mm::frame::{FrameAlloc, FrameAllocOptions, Paddr};

use crate::page_alloc::{
    __alloc_page_frame_raw, __alloc_page_frames_raw, ALLOC_FLAG_DMA, ALLOC_FLAG_NO_PCP,
    free_page_frame,
};

pub struct LegacyFrameAllocShim;

pub static LEGACY_FRAME_ALLOC_SHIM: LegacyFrameAllocShim = LegacyFrameAllocShim;

/// Doubly-indirect handle that `register_frame_allocator` consumes —
/// it requires a `&'static &'static dyn FrameAlloc`, so we materialise
/// the inner reference as a `static` first. `pub` because the boot
/// caller in `boot::boot_memory::boot_step_register_frame_alloc_fn`
/// registers it inline (the former `register_with_ostd(token)` shim
/// has been inlined, taking `&BspToken<'_>` from the boot ctx).
pub static LEGACY_FRAME_ALLOC_DYN: &'static dyn FrameAlloc = &LEGACY_FRAME_ALLOC_SHIM;

impl FrameAlloc for LegacyFrameAllocShim {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr> {
        debug_assert_eq!(
            opts.align_pages, 1,
            "LegacyFrameAllocShim only supports align_pages == 1"
        );
        // The buddy unconditionally scrubs; `opts.zeroing` is now a
        // type-level audit signal (the typestate `Frame<_, Uninit>`
        // path is the documented opt-out for "I will overwrite every
        // byte before reading") rather than a runtime perf escape.
        let mut flags = 0u32;
        if opts.no_pcp {
            flags |= ALLOC_FLAG_NO_PCP;
        }
        if opts.dma {
            flags |= ALLOC_FLAG_DMA;
        }
        let phys = if opts.size_pages <= 1 {
            __alloc_page_frame_raw(flags)
        } else {
            let count = u32::try_from(opts.size_pages).ok()?;
            __alloc_page_frames_raw(count, flags)
        };
        if phys.is_null() { None } else { Some(phys) }
    }

    fn dealloc(&self, paddr: Paddr, _size_pages: usize) {
        let _ = free_page_frame(paddr);
    }
}
