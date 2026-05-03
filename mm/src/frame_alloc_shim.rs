//! Bridges `slopos_ostd::mm::frame::FrameAlloc` to the legacy buddy
//! allocator in [`crate::page_alloc`].
//!
//! Lives in `slopos-mm` (rather than `slopos-ostd`) so `slopos-ostd`
//! has no dependency on `slopos-mm`. Multi-page allocations route to
//! the buddy's `alloc_page_frames` path; the buddy stores the
//! allocation order in the frame descriptor, so `dealloc` recovers
//! it from `paddr` alone and `size_pages` is only validated.

use slopos_ostd::mm::frame::{FrameAlloc, FrameAllocOptions, Paddr};

use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, alloc_page_frames, free_page_frame};

pub struct LegacyFrameAllocShim;

pub static LEGACY_FRAME_ALLOC_SHIM: LegacyFrameAllocShim = LegacyFrameAllocShim;

impl FrameAlloc for LegacyFrameAllocShim {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr> {
        debug_assert_eq!(
            opts.align_pages, 1,
            "LegacyFrameAllocShim only supports align_pages == 1"
        );
        let flags = if opts.zeroing { ALLOC_FLAG_ZERO } else { 0 };
        let phys = if opts.size_pages <= 1 {
            alloc_page_frame(flags)
        } else {
            let count = u32::try_from(opts.size_pages).ok()?;
            alloc_page_frames(count, flags)
        };
        if phys.is_null() { None } else { Some(phys) }
    }

    fn dealloc(&self, paddr: Paddr, _size_pages: usize) {
        let _ = free_page_frame(paddr);
    }
}
