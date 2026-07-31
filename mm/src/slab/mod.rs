//! Kernel slab allocator.
//!
//! Per-size-class [`allocator::SlabAllocator<const SIZE: usize>`]
//! instances + a large-alloc tier (> 2048 bytes → direct frame
//! allocations) aggregated into [`KernelSlab`], the BSS-resident
//! singleton that implements [`slopos_ostd::mm::KernelHeapBackend`]
//! for the `#[global_allocator]` dispatch.
//!
//! Registered with OSTD via
//! [`slopos_ostd::mm::register_kernel_slab_handle`] — same shape as
//! `register_frame_allocator` consumes for `dyn FrameAlloc` (a
//! doubly-indirect `&'static &'static dyn …`).
//!
//! ## Soundness
//!
//! `#![forbid(unsafe_code)]` (inherited from the `mm` crate). All
//! by-pointer access into slab pages goes through OSTD's `ptr_buf`
//! helpers (which carry the residual unsafe). Slab pages live in the
//! kernel HHDM; there is no per-heap virtual-address region.
//!
//! ## Concurrency
//!
//! Each `SlabAllocator<SIZE>` carries its own `SpinLock` over its slab
//! lists, so allocations in different size classes never contend.
//! Per-CPU magazines (one per size class) provide a lock-free fast
//! path. The large-alloc tier has a single `SpinLock` over the free-
//! list head; large allocations are rare relative to slab ones so this
//! is not a hot path.

pub mod allocator;
pub mod compat;
pub mod large;
pub mod magazine;
pub mod page;
pub mod poison;
pub mod stats;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use slopos_ostd::mm::KernelHeapBackend;
use slopos_ostd::sync::BspToken;

pub use compat::{
    HeapStats, get_heap_stats_owned, kernel_heap_enable_diagnostics, kfree, kmalloc, kzalloc,
    print_heap_stats,
};

/// Lifecycle of [`KERNEL_SLAB`]. Encoded as an `AtomicU8` so the
/// transitions stay observable across CPUs and the load-bearing one-
/// shot init can panic on out-of-order callers.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    /// Constructed at link-time; no slab activity allowed yet.
    Uninit = 0,
    /// `init_kernel_slab` has installed the size-class lookup table
    /// and zeroed every class's state. Allocations are live; per-CPU
    /// magazines are still cold (drained).
    Live = 1,
}

impl Lifecycle {
    #[inline]
    const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Per-class size table, indexed by class-id (`0..=7`). The
/// `SlabAllocator<SIZE>` const-generic in
/// [`allocator::SlabAllocator`] makes each class a distinct type, but
/// `KernelSlab::dispatch_alloc` needs a runtime mapping from `size →
/// class_id`, which is what this table provides.
pub(crate) const SIZE_CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];

/// Largest slab size class. Anything strictly larger routes through
/// [`large::LargeAlloc`] (direct multi-page frame allocation).
pub const MAX_SLAB_CLASS_BYTES: usize = 2048;

/// Upper bound on a single kernel allocation (1 MiB).
pub const MAX_ALLOC_SIZE: usize = 0x10_0000;

/// Aggregate slab allocator: 8 per-class slabs + one large-alloc tier
/// + lifecycle state. Sized for BSS — every field is `const`-
/// initialisable so the singleton can be declared as `static
/// KERNEL_SLAB: KernelSlab` without a runtime `init` materialisation
/// step.
pub struct KernelSlab {
    state: AtomicU8,
    /// Per-class slab allocators. Each is its own type because `SIZE`
    /// is a const generic; the runtime `dispatch_*` methods below
    /// switch on `class_id` to reach the right one.
    pub(crate) slab16: allocator::SlabAllocator<16>,
    pub(crate) slab32: allocator::SlabAllocator<32>,
    pub(crate) slab64: allocator::SlabAllocator<64>,
    pub(crate) slab128: allocator::SlabAllocator<128>,
    pub(crate) slab256: allocator::SlabAllocator<256>,
    pub(crate) slab512: allocator::SlabAllocator<512>,
    pub(crate) slab1024: allocator::SlabAllocator<1024>,
    pub(crate) slab2048: allocator::SlabAllocator<2048>,
    pub(crate) large: large::LargeAlloc,
}

impl KernelSlab {
    const fn new_uninit() -> Self {
        Self {
            state: AtomicU8::new(Lifecycle::Uninit.as_u8()),
            slab16: allocator::SlabAllocator::<16>::new_with_class(0),
            slab32: allocator::SlabAllocator::<32>::new_with_class(1),
            slab64: allocator::SlabAllocator::<64>::new_with_class(2),
            slab128: allocator::SlabAllocator::<128>::new_with_class(3),
            slab256: allocator::SlabAllocator::<256>::new_with_class(4),
            slab512: allocator::SlabAllocator::<512>::new_with_class(5),
            slab1024: allocator::SlabAllocator::<1024>::new_with_class(6),
            slab2048: allocator::SlabAllocator::<2048>::new_with_class(7),
            large: large::LargeAlloc::new(),
        }
    }

    #[inline]
    fn is_live(&self) -> bool {
        self.state.load(Ordering::Acquire) == Lifecycle::Live.as_u8()
    }

    /// Map a requested allocation size to a size-class index, or
    /// `None` if the request must route through the large-alloc tier.
    /// The size is the *rounded* size (already aligned up to 16
    /// bytes); callers responsible for the round-up.
    #[inline]
    fn class_of(size: usize) -> Option<usize> {
        let mut idx = 0;
        while idx < SIZE_CLASSES.len() {
            if size <= SIZE_CLASSES[idx] {
                return Some(idx);
            }
            idx += 1;
        }
        None
    }
}

impl KernelHeapBackend for KernelSlab {
    fn alloc(&self, size: usize) -> Option<NonNull<u8>> {
        if !self.is_live() || size == 0 || size > MAX_ALLOC_SIZE {
            return None;
        }
        // VERIFIED (Inv. 10): the 16-byte round-up plus `class_of`'s scan
        // over SIZE_CLASSES always selects a class whose `object_size >=
        // size`, so the returned cell fits any object the caller builds in
        // it. `verification/proofs/slab_lifetime.rs::inv10_into_box_fits`
        // machine-checks this size half (and proves an always-smallest
        // chooser would overflow); the 16-byte cell alignment covers the
        // align half for `align_of::<T>() <= 16` (larger alignments route
        // through the cookie path in `slopos_ostd::mm::heap`).
        let rounded = (size + 15) & !15;
        match Self::class_of(rounded) {
            Some(0) => self.slab16.alloc_one(),
            Some(1) => self.slab32.alloc_one(),
            Some(2) => self.slab64.alloc_one(),
            Some(3) => self.slab128.alloc_one(),
            Some(4) => self.slab256.alloc_one(),
            Some(5) => self.slab512.alloc_one(),
            Some(6) => self.slab1024.alloc_one(),
            Some(7) => self.slab2048.alloc_one(),
            None => self.large.alloc(size),
            // Unreachable: `class_of` only returns `Some(0..=7)`.
            Some(_) => None,
        }
    }

    fn dealloc(&self, ptr: NonNull<u8>) {
        if !self.is_live() {
            return;
        }
        // Discriminate slab vs large via the page header at the
        // 4 KiB-aligned base. Slab pages carry SLAB_MAGIC at offset 0;
        // large-alloc regions carry LARGE_MAGIC. The two magic
        // constants are distinct, so a single peek selects the right
        // tier without consulting an external tracking table.
        match page::page_kind_for(ptr) {
            Some(page::PageKind::Slab { class_idx }) => match class_idx {
                0 => self.slab16.dealloc_one(ptr),
                1 => self.slab32.dealloc_one(ptr),
                2 => self.slab64.dealloc_one(ptr),
                3 => self.slab128.dealloc_one(ptr),
                4 => self.slab256.dealloc_one(ptr),
                5 => self.slab512.dealloc_one(ptr),
                6 => self.slab1024.dealloc_one(ptr),
                7 => self.slab2048.dealloc_one(ptr),
                _ => {}
            },
            Some(page::PageKind::Large) => self.large.dealloc(ptr),
            None => {
                // Pointer is neither a slab object nor a large alloc.
                // Most likely a double free or wild pointer; swallow.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BSS-resident singleton + dyn handle.
// ---------------------------------------------------------------------------

pub(crate) static KERNEL_SLAB: KernelSlab = KernelSlab::new_uninit();

/// Doubly-indirect `&'static dyn KernelHeapBackend`. The OSTD setter
/// (`register_kernel_slab_handle`) takes
/// `&'static &'static dyn KernelHeapBackend`, so we expose the address
/// of this inner static via [`slab_handle`].
static KERNEL_SLAB_DYN: &'static dyn KernelHeapBackend = &KERNEL_SLAB;

/// Stable handle for OSTD registration. Returns the doubly-indirect
/// reference `slopos_ostd::mm::register_kernel_slab_handle` expects.
pub fn slab_handle() -> &'static &'static dyn KernelHeapBackend {
    &KERNEL_SLAB_DYN
}

/// Magazine fast-path arm. Published with `Release` ordering so other
/// CPUs that subsequently observe `true` also observe the slab tier's
/// fully-initialised state. Read with `Acquire` from the hot path.
pub(crate) static HEAP_CACHES_ENABLED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Boot-time lifecycle entry points.
// ---------------------------------------------------------------------------

/// Transition [`KERNEL_SLAB`] from `Uninit` to `Live`. Must run after
/// the buddy allocator + HHDM are live (Memory phase priority ≥ 10).
/// The `&BspToken<'brand>` witness binds the call to the BSP-init
/// scope opened by `slopos_ostd::sync::run_bsp_init`. One-shot;
/// subsequent calls panic.
pub fn init_kernel_slab<'brand>(_token: &BspToken<'brand>) {
    // Single-shot Uninit → Live transition. We CAS so a concurrent
    // boot-time double-invocation panics cleanly rather than silently
    // dropping the second call.
    let prev = KERNEL_SLAB.state.compare_exchange(
        Lifecycle::Uninit.as_u8(),
        Lifecycle::Live.as_u8(),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    assert!(
        prev.is_ok(),
        "slab::init_kernel_slab: state machine not in Uninit"
    );
}

// ============================================================================
// SOFT REBOOT COHERENCY FIX - DO NOT REMOVE
// ============================================================================
//
// After soft reboot (PS/2 0xFE reset), x86 paging structure caches may retain
// stale entries from the previous boot. Limine creates fresh page tables, but
// the CPU's internal paging structure caches aren't automatically coherent.
//
// This causes framebuffer performance to degrade from ~60 FPS to ~1 FPS because:
// 1. Stale paging structure cache entries point to old page table locations
// 2. PAT (Page Attribute Table) settings for Write-Combining aren't applied
// 3. Framebuffer writes fall back to uncached mode (~37,000 cycles/pixel)
//
// The fix requires BOTH:
// - ≥ 2 physical frame allocations: forces the buddy's bitmap/free-list to
//   do read-after-write serialisation on its metadata structures.
// - ≥ 1 kernel-mapping generation bump: forces page-table walks to re-read
//   from Limine's fresh tables instead of the stale paging-structure cache.
//   The HHDM mapping itself was installed by Limine at boot and is always
//   present, so we don't install fresh mappings — the generation bump is
//   sufficient.
//
// References:
// - Intel Application Note 317080-002: "TLBs, Paging-Structure Caches, and
//   Their Invalidation"
// - https://blog.stuffedcow.net/2015/08/pagewalk-coherence/
//
// WARNING: Removing or reducing this below 2 frame allocations WILL cause
// framebuffer performance regression after soft reboot. See
// test_heap_warmup_pages_minimum() in mm/src/tests/tests.rs.
// ============================================================================
pub const HEAP_WARMUP_PAGES: u32 = 4;

/// Perform the soft-reboot coherency warmup. See the comment block
/// above for the full motivation; this function is the load-bearing
/// site for **framebuffer perf parity across soft reboot**.
pub fn warmup_for_soft_reboot() {
    use slopos_ostd::mm::frame::{Frame, KernelMeta};
    // Hold the temporary frames so they're all freed together when this
    // function returns. Holding them in a stack array (not a `KVec`)
    // avoids any risk of the slab recursing into itself during warmup.
    let mut held: [Option<Frame<KernelMeta>>; HEAP_WARMUP_PAGES as usize] =
        [const { None }; HEAP_WARMUP_PAGES as usize];
    for slot in held.iter_mut() {
        *slot = Frame::<KernelMeta>::alloc_zeroed();
    }
    // `held` drops here; the frames return to the buddy.
}

/// Arm the per-CPU magazine fast path. Idempotent. Called from
/// `memory_init.rs` after the slab is live and the SMP per-CPU areas
/// have been brought online.
pub fn enable_heap_caches() {
    HEAP_CACHES_ENABLED.store(true, Ordering::Release);
}

/// Drain every per-CPU magazine. Used by the test harness's heap
/// reinit path so stale per-CPU pointers cannot be handed out after a
/// reset.
pub fn drain_all_heap_caches() {
    if !HEAP_CACHES_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    KERNEL_SLAB.slab16.drain_magazines();
    KERNEL_SLAB.slab32.drain_magazines();
    KERNEL_SLAB.slab64.drain_magazines();
    KERNEL_SLAB.slab128.drain_magazines();
    KERNEL_SLAB.slab256.drain_magazines();
    KERNEL_SLAB.slab512.drain_magazines();
    KERNEL_SLAB.slab1024.drain_magazines();
    KERNEL_SLAB.slab2048.drain_magazines();
}
