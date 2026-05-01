//! Sealed `StackRegion` trait describing one fixed-stride VA region used
//! for task-owned stacks, plus the ZST tag types that select between
//! regions.
//!
//! # Why a trait
//!
//! Both the kernel-stack and unsafe-stack VA regions are managed by
//! identical bitmap + per-CPU-cache machinery; they only differ in five
//! constants (`*_VA_BASE`, `*_VA_END`, `*_STRIDE`, `*_GUARD_SIZE`,
//! `*_MAX_SLOTS`).  Parameterising the allocator and the RAII slot
//! handle over a `StackRegion` trait collapses ~1500 LOC of mechanical
//! K↔U mirror code into a single generic implementation while keeping
//! every region's allocator state in a separate static (so the two
//! locks remain independent and the warm path stays lock-free).
//!
//! # Why sealed
//!
//! Adding a new VA region demands coordination with `memory_layout_defs`
//! (carving out a window) and with the page-table sentinel install
//! (which assumes 2 MB-aligned chunks).  Sealing the trait forces every
//! new region to be added inside the `mm` crate, where those invariants
//! are visible.
//!
//! # Why uninhabited tag types
//!
//! `KstackRegion` and `UstackRegion` are `enum`s with no variants, so
//! they are `Sized` (bound by `'static`), have size 0, and **cannot** be
//! instantiated.  They exist purely as compile-time labels carried by
//! `StackSlot<R>` / `TaskStack<R>` via `PhantomData<fn() -> R>`.  This
//! is what gives a `KstackSlot` and a `UstackSlot` distinct nominal
//! types at zero runtime cost — a class of bug C kernels can only catch
//! at runtime via cookies or by convention.
//!
//! # Per-region wiring
//!
//! The trait carries hidden `_*` methods used by the generic allocator
//! to reach a region's per-region statics (the global bitmap mutex and
//! the per-CPU cache array).  Each region's instantiation module
//! ([`crate::kstack_va`], [`crate::ustack_va`]) declares those statics
//! and implements the wiring methods.  User code never calls the `_*`
//! methods directly — they are invoked by [`StackSlot::drop`] and by
//! the public `crate::stack_va::*` helpers.

use crate::stack_va::{PcpStats, SlotEntry};

mod sealed {
    pub trait Sealed {}
}

/// One fixed-stride VA region used as a pool of task stacks.
///
/// Implementors are uninhabited tag enums (e.g. [`KstackRegion`]).  The
/// trait is sealed; new regions must be added in the `mm` crate.
pub trait StackRegion: 'static + sealed::Sealed {
    /// Short name for diagnostics (e.g. `"kstack"`).
    const NAME: &'static str;

    /// Lowest VA in the region (inclusive).
    const VA_BASE: u64;

    /// VA one past the region's end (exclusive).
    const VA_END: u64;

    /// Per-slot VA stride.  Must be a power of two ≥ `PAGE_SIZE_4KB`.
    const STRIDE: u64;

    /// Guard-page size carved off the bottom of every slot.  Must be a
    /// multiple of `PAGE_SIZE_4KB` and strictly less than `STRIDE`.
    const GUARD_SIZE: u64;

    /// Number of slots in the region — equals `(VA_END - VA_BASE) / STRIDE`.
    /// Spelled separately so the `[u64; BITMAP_WORDS]` const generic on
    /// the global allocator can reference it via `BITMAP_WORDS` without
    /// requiring `feature(generic_const_exprs)`.
    const MAX_SLOTS: usize;

    /// `(MAX_SLOTS + 63) / 64`.  Spelled explicitly for stable const-
    /// generic arity on `StackVaAllocator<R, WORDS>`.  A compile-time
    /// `const _` cross-check inside the allocator catches drift.
    const BITMAP_WORDS: usize;

    /// Per-CPU cache capacity (slots).  16 today.
    const PCP_CAPACITY: usize;

    /// Slots drained from the global allocator into a CPU's cache per
    /// refill.  Must be ≤ `PCP_CAPACITY`.
    const PCP_REFILL_BATCH: usize;

    /// Slots returned to the global allocator when a CPU's cache is
    /// full.  Must be strictly less than `PCP_CAPACITY` so a subsequent
    /// push cannot immediately overflow again.
    const PCP_SPILL_BATCH: usize;

    /// Page flags used when mapping physical frames into a slot.  Stored
    /// as the raw bitfield since `PageFlags` itself is not a primitive
    /// type and so cannot be a const generic.
    const PAGE_FLAGS: u64;

    // ---- Per-region wiring (implemented by the instantiation module) -----
    //
    // These methods exist because each region owns its own set of
    // `static`s (one `IrqMutex<StackVaAllocator<…>>` plus one
    // `PcpArray<…>`).  The generic allocator routes calls through these
    // hidden trait methods to reach the right statics — this is what
    // keeps the two regions' lock contention independent while sharing
    // one implementation.

    /// Pop one slot from the current CPU's cache (refilling from the
    /// global if empty).  Called by [`crate::stack_va::alloc_slot`].
    #[doc(hidden)]
    fn _slot_pop() -> Option<SlotEntry>;

    /// Push one slot back to the current CPU's cache (spilling to the
    /// global if the cache is full).  Called by
    /// [`crate::stack_va::StackSlot::drop`].
    #[doc(hidden)]
    fn _slot_push(entry: SlotEntry);

    /// Drain the current CPU's cache back to the global.  Test-only.
    #[doc(hidden)]
    fn _pcp_flush_current();

    /// Drain every CPU's cache back to the global.  Shutdown only.
    #[doc(hidden)]
    fn _pcp_drain_all();

    /// Diagnostic snapshot of one CPU's cache state.
    #[doc(hidden)]
    fn _pcp_stats(cpu: usize) -> PcpStats;

    /// Slots currently held outside the global free pool — sum over
    /// dropped-into-PCP and live `StackSlot<Self>` handles.
    #[doc(hidden)]
    fn _in_use_count() -> u32;

    /// Initialise the global bitmap and install page-table sentinels.
    /// Called once at boot via [`crate::stack_va::init`].
    #[doc(hidden)]
    fn _init();
}

/// VA region for kernel-mode task stacks.
///
/// See `KSTACK_VA_BASE..KSTACK_VA_END` in [`crate::memory_layout_defs`].
/// Concrete `StackRegion` impl + per-region statics live in
/// [`crate::kstack_va`].
pub enum KstackRegion {}
impl sealed::Sealed for KstackRegion {}

/// VA region for SafeStack-sanitiser data ("unsafe") stacks.
///
/// Disjoint from [`KstackRegion`] so a stray data-stack OOB cannot
/// rewrite control flow on the safe (kernel) stack.  See
/// `USTACK_VA_BASE..USTACK_VA_END` in [`crate::memory_layout_defs`].
/// Concrete `StackRegion` impl + per-region statics live in
/// [`crate::ustack_va`].
pub enum UstackRegion {}
impl sealed::Sealed for UstackRegion {}
