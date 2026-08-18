//! Kernel slab allocator.
//!
//! Per-size-class [`allocator::SlabAllocator<const SIZE: usize>`]
//! instances + a large-alloc tier (> 2048 bytes → direct frame
//! allocations) aggregated into [`KernelSlab`], the BSS-resident
//! singleton that implements [`slopos_ostd::mm::KernelHeapBackend`]
//! for the `#[global_allocator]` dispatch.
//!
//! Slab pages live in the kernel HHDM; there is no per-heap
//! virtual-address region. Each `SlabAllocator<SIZE>` carries its own
//! `SpinLock` over its slab lists, so allocations in different size
//! classes never contend, and per-CPU magazines provide a lock-free
//! fast path.

pub mod allocator;
pub mod compat;
pub mod large;
pub mod magazine;
pub mod page;
pub mod poison;
pub mod stats;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use slopos_ostd::lock_class;
use slopos_ostd::mm::KernelHeapBackend;
use slopos_ostd::sync::{BspToken, LOCK_LEVEL_ALLOCATOR};

pub use compat::{
    HeapStats, get_heap_stats_owned, kernel_heap_enable_diagnostics, kfree, kmalloc, kzalloc,
    print_heap_stats,
};

/// Lifecycle of [`KERNEL_SLAB`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    /// Constructed at link-time; no slab activity allowed yet.
    Uninit = 0,
    /// Allocations are live; per-CPU magazines are still cold (drained).
    Live = 1,
}

impl Lifecycle {
    #[inline]
    const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Per-class size table, indexed by class-id (`0..=7`): the runtime
/// `size → class_id` mapping the const-generic class types cannot provide.
pub(crate) const SIZE_CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];

/// Largest slab size class. Anything strictly larger routes through
/// [`large::LargeAlloc`] (direct multi-page frame allocation).
pub const MAX_SLAB_CLASS_BYTES: usize = 2048;

/// Upper bound on a single kernel allocation (1 MiB).
pub const MAX_ALLOC_SIZE: usize = 0x10_0000;

/// Aggregate slab allocator: 8 per-class slabs, one large-alloc tier, and the
/// lifecycle state. Every field is `const`-initialisable so the singleton can
/// live in BSS without a runtime materialisation step.
pub struct KernelSlab {
    state: AtomicU8,
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
            slab16: allocator::SlabAllocator::<16>::new_with_class(
                0,
                lock_class!("SLAB_16", LOCK_LEVEL_ALLOCATOR),
            ),
            slab32: allocator::SlabAllocator::<32>::new_with_class(
                1,
                lock_class!("SLAB_32", LOCK_LEVEL_ALLOCATOR),
            ),
            slab64: allocator::SlabAllocator::<64>::new_with_class(
                2,
                lock_class!("SLAB_64", LOCK_LEVEL_ALLOCATOR),
            ),
            slab128: allocator::SlabAllocator::<128>::new_with_class(
                3,
                lock_class!("SLAB_128", LOCK_LEVEL_ALLOCATOR),
            ),
            slab256: allocator::SlabAllocator::<256>::new_with_class(
                4,
                lock_class!("SLAB_256", LOCK_LEVEL_ALLOCATOR),
            ),
            slab512: allocator::SlabAllocator::<512>::new_with_class(
                5,
                lock_class!("SLAB_512", LOCK_LEVEL_ALLOCATOR),
            ),
            slab1024: allocator::SlabAllocator::<1024>::new_with_class(
                6,
                lock_class!("SLAB_1024", LOCK_LEVEL_ALLOCATOR),
            ),
            slab2048: allocator::SlabAllocator::<2048>::new_with_class(
                7,
                lock_class!("SLAB_2048", LOCK_LEVEL_ALLOCATOR),
            ),
            large: large::LargeAlloc::new(),
        }
    }

    #[inline]
    fn is_live(&self) -> bool {
        self.state.load(Ordering::Acquire) == Lifecycle::Live.as_u8()
    }

    /// Size-class index for a request, or `None` if it must route through the
    /// large-alloc tier. `size` must already be rounded up to 16 bytes.
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
        // VERIFIED (Inv. 10): the 16-byte round-up plus `class_of`'s scan over
        // SIZE_CLASSES always selects a class whose `object_size >= size`,
        // machine-checked by
        // `verification/proofs/slab_lifetime.rs::inv10_into_box_fits`. The
        // 16-byte cell alignment covers `align_of::<T>() <= 16`; larger
        // alignments route through the cookie path in `slopos_ostd::mm::heap`.
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
                // A wild or double free; swallow.
            }
        }
    }
}

pub(crate) static KERNEL_SLAB: KernelSlab = KernelSlab::new_uninit();

/// Inner static behind [`slab_handle`]; OSTD's `register_kernel_slab_handle`
/// takes a doubly-indirect `&'static &'static dyn KernelHeapBackend`.
static KERNEL_SLAB_DYN: &'static dyn KernelHeapBackend = &KERNEL_SLAB;

/// Stable handle for OSTD registration.
pub fn slab_handle() -> &'static &'static dyn KernelHeapBackend {
    &KERNEL_SLAB_DYN
}

/// Magazine fast-path arm. Published `Release` so a CPU that observes `true`
/// also observes the slab tier's fully-initialised state.
pub(crate) static HEAP_CACHES_ENABLED: AtomicBool = AtomicBool::new(false);

/// Transition [`KERNEL_SLAB`] from `Uninit` to `Live`. Must run after
/// the buddy allocator + HHDM are live (Memory phase priority ≥ 10).
/// The `&BspToken<'brand>` witness binds the call to the BSP-init
/// scope opened by `slopos_ostd::sync::run_bsp_init`. One-shot;
/// subsequent calls panic.
pub fn init_kernel_slab<'brand>(_token: &BspToken<'brand>) {
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

// After a soft reboot (PS/2 0xFE reset) the CPU's paging-structure caches may
// retain entries from the previous boot even though Limine builds fresh page
// tables, and the framebuffer's Write-Combining PAT setting is then not
// applied. Recovering needs both ≥ 2 physical frame allocations (which force
// read-after-write serialisation on the buddy's metadata) and ≥ 1
// kernel-mapping generation bump (which forces walks to re-read Limine's fresh
// tables). Intel Application Note 317080-002, "TLBs, Paging-Structure Caches,
// and Their Invalidation". Floor of 2 guarded by
// `test_heap_warmup_pages_minimum`.
pub const HEAP_WARMUP_PAGES: u32 = 4;

/// Perform the soft-reboot coherency warmup; see [`HEAP_WARMUP_PAGES`].
pub fn warmup_for_soft_reboot() {
    use slopos_ostd::mm::frame::{Frame, KernelMeta};
    // A stack array rather than a `KVec`: the warmup must not recurse into the
    // slab it is warming.
    let mut held: [Option<Frame<KernelMeta>>; HEAP_WARMUP_PAGES as usize] =
        [const { None }; HEAP_WARMUP_PAGES as usize];
    for slot in held.iter_mut() {
        *slot = Frame::<KernelMeta>::alloc_zeroed();
    }
}

/// Arm the per-CPU magazine fast path. Idempotent; requires the slab live and
/// the SMP per-CPU areas online.
pub fn enable_heap_caches() {
    HEAP_CACHES_ENABLED.store(true, Ordering::Release);
}

/// Drain every per-CPU magazine, so a heap reinit cannot hand out stale
/// per-CPU pointers.
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
