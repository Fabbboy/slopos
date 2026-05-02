//! Bridges `slopos_ostd::mm::frame::FrameAlloc` to the legacy buddy
//! allocator in [`crate::page_alloc`].
//!
//! Lives in `slopos-mm` (rather than `slopos-ostd`) so `slopos-ostd`
//! has no dependency on `slopos-mm`. Only single-page, page-aligned
//! allocations are supported; multi-page allocations will land with
//! the `USegment` work.

use slopos_ostd::mm::frame::{FrameAlloc, FrameAllocOptions, Paddr};

use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame};

pub struct LegacyFrameAllocShim;

pub static LEGACY_FRAME_ALLOC_SHIM: LegacyFrameAllocShim = LegacyFrameAllocShim;

impl FrameAlloc for LegacyFrameAllocShim {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr> {
        debug_assert_eq!(
            opts.size_pages, 1,
            "LegacyFrameAllocShim only supports size_pages == 1"
        );
        debug_assert_eq!(
            opts.align_pages, 1,
            "LegacyFrameAllocShim only supports align_pages == 1"
        );
        let flags = if opts.zeroing { ALLOC_FLAG_ZERO } else { 0 };
        let phys = alloc_page_frame(flags);
        if phys.is_null() { None } else { Some(phys) }
    }

    fn dealloc(&self, paddr: Paddr, _size_pages: usize) {
        let _ = free_page_frame(paddr);
    }
}
