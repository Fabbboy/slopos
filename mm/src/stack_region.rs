//! Sealed `StackRegion` trait describing one fixed-stride VA region used for
//! task-owned stacks, plus the uninhabited tag types that select between
//! regions. Both regions share one bitmap + per-CPU-cache implementation and
//! differ only in their constants, but each keeps its allocator state in its
//! own statics so the two locks stay independent.
//!
//! Sealing forces a new region to be added inside the `mm` crate, where its
//! coordination with `memory_layout_defs` and with the 2 MB-aligned page-table
//! sentinel install is visible.
//!
//! The hidden `_*` methods are how the generic allocator reaches a region's own
//! statics; each region's instantiation module ([`crate::kstack_va`],
//! [`crate::ustack_va`]) implements them, and no caller invokes them directly.

use crate::stack_va::{PcpStats, SlotEntry};

mod sealed {
    pub trait Sealed {}
}

/// One fixed-stride VA region used as a pool of task stacks. Implementors are
/// uninhabited tag enums (e.g. [`KstackRegion`]).
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

    /// `(VA_END - VA_BASE) / STRIDE`, spelled separately so the
    /// `[u64; BITMAP_WORDS]` const generic on the global allocator can
    /// reference it without `feature(generic_const_exprs)`.
    const MAX_SLOTS: usize;

    /// `(MAX_SLOTS + 63) / 64`, spelled explicitly for stable const-generic
    /// arity on `StackVaAllocator<R, WORDS>`; a `const _` cross-check inside
    /// the allocator catches drift.
    const BITMAP_WORDS: usize;

    /// Per-CPU cache capacity, in slots.
    const PCP_CAPACITY: usize;

    /// Slots drained from the global allocator into a CPU's cache per
    /// refill.  Must be ≤ `PCP_CAPACITY`.
    const PCP_REFILL_BATCH: usize;

    /// Slots returned to the global allocator when a CPU's cache is
    /// full.  Must be strictly less than `PCP_CAPACITY` so a subsequent
    /// push cannot immediately overflow again.
    const PCP_SPILL_BATCH: usize;

    /// Page flags used when mapping physical frames into a slot. A raw
    /// bitfield because `PageFlags` is not a primitive and so cannot be a
    /// const generic.
    const PAGE_FLAGS: u64;

    /// Pop one slot from the current CPU's cache, refilling from the global if
    /// it is empty.
    #[doc(hidden)]
    fn _slot_pop() -> Option<SlotEntry>;

    /// Push one slot back to the current CPU's cache, spilling to the global if
    /// it is full.
    #[doc(hidden)]
    fn _slot_push(entry: SlotEntry);

    /// Drain the current CPU's cache back to the global.  Test-only.
    #[doc(hidden)]
    fn _pcp_flush_current();

    /// Drain every CPU's cache back to the global.  Shutdown only.
    #[doc(hidden)]
    fn _pcp_drain_all();

    #[doc(hidden)]
    fn _pcp_stats(cpu: usize) -> PcpStats;

    /// Slots currently held outside the global free pool — sum over
    /// dropped-into-PCP and live `StackSlot<Self>` handles.
    #[doc(hidden)]
    fn _in_use_count() -> u32;

    /// Initialise the global bitmap and install page-table sentinels; run once
    /// at boot.
    #[doc(hidden)]
    fn _init();
}

/// VA region for kernel-mode task stacks: `KSTACK_VA_BASE..KSTACK_VA_END` in
/// [`crate::memory_layout_defs`], implemented in [`crate::kstack_va`].
pub enum KstackRegion {}
impl sealed::Sealed for KstackRegion {}

/// VA region for SafeStack-sanitiser data stacks, disjoint from
/// [`KstackRegion`] so a stray data-stack OOB cannot rewrite control flow on
/// the safe (kernel) stack: `USTACK_VA_BASE..USTACK_VA_END` in
/// [`crate::memory_layout_defs`], implemented in [`crate::ustack_va`].
pub enum UstackRegion {}
impl sealed::Sealed for UstackRegion {}
