//! Bridges `slopos_ostd::mm::frame::FrameAlloc` to the legacy buddy
//! allocator in [`crate::page_alloc`].
//!
//! Lives in `slopos-mm` (rather than `slopos-ostd`) so `slopos-ostd`
//! has no dependency on `slopos-mm`. Multi-page allocations route to
//! the buddy's `alloc_page_frames` path; the buddy stores the
//! allocation order in the frame descriptor, so `dealloc` recovers
//! it from `paddr` alone and `size_pages` is only validated.

use slopos_ostd::mm::frame::{FrameAlloc, FrameAllocOptions, Paddr};
use slopos_ostd::mm::frame_alloc::register_frame_allocator;

use crate::page_alloc::{alloc_page_frame, alloc_page_frames, free_page_frame};

pub struct LegacyFrameAllocShim;

pub static LEGACY_FRAME_ALLOC_SHIM: LegacyFrameAllocShim = LegacyFrameAllocShim;

/// Doubly-indirect handle that `register_frame_allocator` consumes —
/// it requires a `&'static &'static dyn FrameAlloc`, so we materialise
/// the inner reference as a `static` first.
pub static LEGACY_FRAME_ALLOC_DYN: &'static dyn FrameAlloc = &LEGACY_FRAME_ALLOC_SHIM;

impl FrameAlloc for LegacyFrameAllocShim {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr> {
        debug_assert_eq!(
            opts.align_pages, 1,
            "LegacyFrameAllocShim only supports align_pages == 1"
        );
        let flags = if opts.zeroing { 0 } else { 0 };
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

/// Boot hook: register the legacy buddy allocator with OSTD so
/// `VmSpace::new`, `Frame::<M>::from_unused`, and other OSTD primitives
/// that allocate physical frames work.
///
/// Must run after the buddy allocator is up (i.e. after the Memory
/// boot phase).  Safe to call only once — `register_frame_allocator`
/// asserts on double-registration.
///
/// # Safety
/// Caller guarantees this is the only registration site for the OSTD
/// frame allocator in the production boot path.
pub unsafe fn register_with_ostd() {
    unsafe {
        register_frame_allocator(&LEGACY_FRAME_ALLOC_DYN);
    }
}
