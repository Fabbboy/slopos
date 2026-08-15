//! Physical page frame allocator (buddy + per-CPU caches).
//!
//! The kernel's safe-Rust [`FrameAlloc`] implementation lives here.
//! OSTD's frame-allocation API consults the registered allocator via
//! [`slopos_ostd::mm::frame_alloc::register_frame_allocator`]; boot
//! hands it [`frame_alloc_handle`] which points at the BSS-resident
//! [`BUDDY_ALLOCATOR`] singleton.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │              Frame::<KernelMeta>::alloc(opts)                   │
//! │                           │                                     │
//! │                           ▼                                     │
//! │              current_frame_allocator()?.alloc(opts)             │
//! │                           │                                     │
//! │                           ▼                                     │
//! │              <BuddyAllocator as FrameAlloc>::alloc              │
//! │                           │                                     │
//! │              ┌────────────┴────────────┐                        │
//! │              │    Order == 0?          │                        │
//! │              └────────────┬────────────┘                        │
//! │                   Yes     │      No                             │
//! │              ┌────────────┴────────────┐                        │
//! │              ▼                         ▼                        │
//! │   ┌─────────────────────┐   ┌─────────────────────┐             │
//! │   │ Per-CPU Page Cache  │   │   Buddy Allocator   │             │
//! │   │   (lock-free)       │   │   (global lock)     │             │
//! │   └─────────┬───────────┘   └─────────────────────┘             │
//! │             │ Empty?                                            │
//! │             ▼                                                   │
//! │   ┌─────────────────────┐                                       │
//! │   │ Refill from buddy   │                                       │
//! │   └─────────────────────┘                                       │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! See [`buddy`] for the allocator type and [`pcp`] for the per-CPU
//! cache data layer.

pub mod buddy;
mod pcp;

use core::ffi::c_int;

use slopos_abi::addr::PhysAddr;
use slopos_arch::pcr::MAX_CPUS;
use slopos_ostd::mm::frame::FrameAlloc;

pub use buddy::{
    ALLOC_FLAG_DMA, ALLOC_FLAG_KERNEL, ALLOC_FLAG_NO_PCP, ALLOC_FLAG_ORDER_MASK,
    ALLOC_FLAG_ORDER_SHIFT, BuddyAllocator,
};

// ---------------------------------------------------------------------------
// The single global instance.
// ---------------------------------------------------------------------------

/// BSS-resident buddy allocator. Drives the kernel's physical page
/// supply once boot has driven the lifecycle through `install_descriptor_table
/// → seed_from_memory_map → enable_pcp`. The `Send + Sync` bounds on
/// [`FrameAlloc`] are satisfied by the type's interior locking.
pub static BUDDY_ALLOCATOR: BuddyAllocator = BuddyAllocator::new_uninit();

/// Doubly-indirect reference for
/// [`slopos_ostd::mm::frame_alloc::register_frame_allocator`]; the
/// setter requires a `&'static &'static dyn FrameAlloc` so the inner
/// reference must live in a `static`.
static BUDDY_ALLOCATOR_DYN: &dyn FrameAlloc = &BUDDY_ALLOCATOR;

/// Hand boot the static reference it needs to pass to OSTD's
/// `register_frame_allocator`. Stable address; safe to call any time
/// after link.
#[inline]
pub fn frame_alloc_handle() -> &'static &'static dyn FrameAlloc {
    &BUDDY_ALLOCATOR_DYN
}

// ---------------------------------------------------------------------------
// Public API. Names preserved from the pre-refactor flat module so
// callers outside `mm/` are unaffected; bodies are thin wrappers over
// the static [`BUDDY_ALLOCATOR`].
// ---------------------------------------------------------------------------

/// Raw multi-page buddy entry point. Bootstrap escape for
/// `kernel_meta::install_meta_slots` (which runs before the OSTD
/// frame-allocator registration is live) and policy-flag opt-out
/// (`ALLOC_FLAG_NO_PCP`, `ALLOC_FLAG_DMA`) for callers that bypass
/// the typestate.
#[doc(hidden)]
pub fn __alloc_page_frames_raw(count: u32, flags: u32) -> PhysAddr {
    BUDDY_ALLOCATOR.alloc_raw(count, flags)
}

/// Raw single-page buddy entry point. See [`__alloc_page_frames_raw`]
/// for the audit-point rationale.
#[doc(hidden)]
pub fn __alloc_page_frame_raw(flags: u32) -> PhysAddr {
    BUDDY_ALLOCATOR.alloc_raw(1, flags)
}

/// Typestate-checked single-page kernel allocation, zeroed.
pub fn alloc_kernel_page() -> PhysAddr {
    use slopos_ostd::mm::frame::{Frame, FrameAllocOptions, KernelMeta};
    Frame::<KernelMeta>::alloc_release_phys(FrameAllocOptions::single())
}

/// Typestate-checked single-page kernel allocation with caller-supplied options.
pub fn alloc_kernel_page_with(opts: slopos_ostd::mm::frame::FrameAllocOptions) -> PhysAddr {
    use slopos_ostd::mm::frame::{Frame, KernelMeta};
    Frame::<KernelMeta>::alloc_release_phys(opts)
}

/// Typestate-checked multi-page kernel allocation.
pub fn alloc_kernel_pages(count: u32) -> PhysAddr {
    use slopos_ostd::mm::frame::{Frame, FrameAllocOptions, KernelMeta};
    if count == 0 {
        return PhysAddr::NULL;
    }
    let opts = FrameAllocOptions {
        size_pages: count as usize,
        ..FrameAllocOptions::single()
    };
    Frame::<KernelMeta>::alloc_release_phys(opts)
}

/// Typestate-checked multi-page kernel allocation with caller-supplied options.
pub fn alloc_kernel_pages_with(
    count: u32,
    opts: slopos_ostd::mm::frame::FrameAllocOptions,
) -> PhysAddr {
    use slopos_ostd::mm::frame::{Frame, KernelMeta};
    if count == 0 {
        return PhysAddr::NULL;
    }
    let opts = slopos_ostd::mm::frame::FrameAllocOptions {
        size_pages: count as usize,
        ..opts
    };
    Frame::<KernelMeta>::alloc_release_phys(opts)
}

/// Batch-allocate up to `out.len()` zeroed order-0 pages.
pub fn alloc_page_frames_pcp_batch(out: &mut [PhysAddr]) -> usize {
    BUDDY_ALLOCATOR.alloc_pcp_batch(out)
}

/// Free a single allocation (single page or multi-page block) back
/// to the buddy. The buddy recovers the order from the descriptor.
pub fn free_page_frame(phys_addr: PhysAddr) -> c_int {
    BUDDY_ALLOCATOR.free_phys(phys_addr)
}

/// Drain every CPU's PCP cache into the buddy. Shutdown only.
pub fn pcp_drain_all() {
    BUDDY_ALLOCATOR.drain_pcp_all();
}

/// Promote the batch the closing epoch proved safe. Called by
/// [`crate::mmu::quiesce`] from whichever CPU closes the epoch — so it must
/// stay O(1); see `BuddyAllocator::quarantine_rotate`.
pub fn quarantine_rotate() {
    BUDDY_ALLOCATOR.quarantine_rotate();
}

/// Splice up to `limit` proven-safe blocks back into the free lists, from
/// ordinary context. Returns the frames released.
pub fn quarantine_release_some(limit: u32) -> u32 {
    BUDDY_ALLOCATOR.quarantine_release_some(limit)
}

/// Is there proven-safe memory waiting to be spliced back into the free lists?
pub fn quarantine_has_releasable() -> bool {
    BUDDY_ALLOCATOR.quarantine_has_releasable()
}

/// Is any memory currently parked awaiting a TLB quiesce?
pub fn quarantine_is_occupied() -> bool {
    BUDDY_ALLOCATOR.quarantine_is_occupied()
}

/// Frames currently parked awaiting a TLB quiesce.
pub fn quarantine_frames() -> u32 {
    BUDDY_ALLOCATOR.quarantine_frames()
}

// ---------------------------------------------------------------------------
// Stats / diagnostic accessors.
// ---------------------------------------------------------------------------

pub fn page_allocator_descriptor_size() -> usize {
    core::mem::size_of::<buddy::PageFrame>()
}

pub fn page_allocator_max_supported_frames() -> u32 {
    BUDDY_ALLOCATOR.max_supported_frames()
}

/// Frame counts across the whole buddy allocator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageAllocatorStats {
    pub total: u32,
    pub free: u32,
    pub allocated: u32,
}

/// One CPU's per-CPU page-cache counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcpStats {
    pub count: u32,
    pub allocs: u32,
    pub frees: u32,
}

pub fn get_page_allocator_stats() -> PageAllocatorStats {
    let (total, free, allocated) = BUDDY_ALLOCATOR.stats();
    PageAllocatorStats {
        total,
        free,
        allocated,
    }
}

/// `None` for an out-of-range CPU, or one whose per-CPU cache is not up.
pub fn get_pcp_stats(cpu: usize) -> Option<PcpStats> {
    if cpu >= MAX_CPUS {
        return None;
    }
    BUDDY_ALLOCATOR
        .pcp_stats(cpu)
        .map(|(count, allocs, frees)| PcpStats {
            count,
            allocs,
            frees,
        })
}

pub fn page_frame_is_tracked(phys_addr: PhysAddr) -> c_int {
    BUDDY_ALLOCATOR.frame_is_tracked(phys_addr) as c_int
}

pub fn page_allocator_paint_all(value: u8) {
    BUDDY_ALLOCATOR.paint_all(value);
}

// ---------------------------------------------------------------------------
// OwnedPageFrame typestate alias.
// ---------------------------------------------------------------------------

/// Owning handle to a single 4 KiB kernel-owned physical frame.
///
/// Aliased onto `slopos_ostd::mm::frame::Frame<KernelMeta>` so the
/// underlying ref-counted slot machinery from OSTD drives the
/// allocate/free lifecycle. The kernel-side allocator
/// ([`BUDDY_ALLOCATOR`]) is registered with OSTD through
/// [`frame_alloc_handle`], and the final
/// [`slopos_ostd::mm::frame::Frame`] drop routes back into
/// [`free_page_frame`] via OSTD's `KernelMeta::on_drop`.
pub type OwnedPageFrame = slopos_ostd::mm::frame::Frame<crate::kernel_meta::KernelMeta>;

pub use OwnedPageFrame as KernelFrame;

// ---------------------------------------------------------------------------
// Reclaim
// ---------------------------------------------------------------------------

/// The TLB quarantine as a reclaim source.
///
/// Frames sit here after being unmapped, waiting for every CPU to prove it has
/// invalidated its translation. Once the epoch closes they are *already free*
/// — nothing references them and no work is needed to release them beyond
/// splicing them back into the free lists. That makes this the cheapest and
/// most certainly-recoverable pool in the kernel, so it is asked first.
struct QuarantineReclaim;

impl slopos_ostd::mm::reclaim::Reclaimable for QuarantineReclaim {
    fn name(&self) -> &'static str {
        "tlb-quarantine"
    }

    fn reclaimable_pages(&self) -> u32 {
        if BUDDY_ALLOCATOR.quarantine_has_releasable() {
            BUDDY_ALLOCATOR.quarantine_frames()
        } else {
            0
        }
    }

    fn reclaim(&self, want: u32) -> u32 {
        BUDDY_ALLOCATOR.quarantine_release_some(want)
    }
}

static QUARANTINE_RECLAIM: QuarantineReclaim = QuarantineReclaim;

/// Register the quarantine with the reclaim tier. Boot only.
pub fn register_reclaim(token: &slopos_ostd::sync::BspToken<'_>) {
    slopos_ostd::mm::reclaim::register(token, &QUARANTINE_RECLAIM);
}
