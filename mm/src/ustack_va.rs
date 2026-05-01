//! Unsafe-stack virtual-address allocator — thin instantiation of the
//! generic [`crate::stack_va`] core for the [`UstackRegion`] tag.
//!
//! Manages the SafeStack data-stack VA region
//! (`USTACK_VA_BASE..USTACK_VA_END`).  The U region deliberately sits in
//! a disjoint VA range from the K (kernel-stack) region so an
//! address-taken pointer that leaks from the unsafe stack cannot alias
//! the safe (kernel) stack — which is what makes the SafeStack
//! partitioning an effective ROP defence.
//!
//! All algorithmic logic lives in [`crate::stack_va`].  This file
//! contributes:
//!
//! - The [`UstackRegion`] `StackRegion` trait impl (constants + wiring).
//! - Two private statics: the global bitmap allocator and the per-CPU
//!   cache array.  They are intentionally separate from the K-region
//!   statics so the two locks contend independently and a churn storm
//!   on one region does not stall the other.
//! - Compatibility re-exports under the historical `ustack_va::*` names
//!   so existing callers (`safestack_rt`, scheduler tests) keep
//!   compiling unchanged.

use slopos_sync::{IrqMutex, LOCK_LEVEL_ALLOCATOR};

use crate::memory_layout_defs::{USTACK_MAX_SLOTS, USTACK_STRIDE, USTACK_VA_BASE, USTACK_VA_END};
use crate::paging_defs::PageFlags;
use crate::stack_region::{StackRegion, UstackRegion};
use crate::stack_va::{
    PcpArray, PcpStats, PerCpuStackCache, SlotEntry, StackSlot, StackVaAllocator,
    init_with_sentinels, pcp_drain_all_impl, pcp_flush_current_impl, slot_pop_impl, slot_push_impl,
};

// ---------------------------------------------------------------------------
// Region constants — the only knobs that distinguish U from K.
// ---------------------------------------------------------------------------

const BITMAP_WORDS: usize = USTACK_MAX_SLOTS.div_ceil(64);
const PCP_CAPACITY: usize = 16;
const PCP_REFILL_BATCH: usize = 8;
const PCP_SPILL_BATCH: usize = 8;
const PAGE_FLAGS: u64 = PageFlags::KERNEL_RW.bits() | PageFlags::NO_EXECUTE.bits();

// Compile-time region-shape sanity (matches what the old hand-rolled
// allocator asserted).
const _: () = {
    let span = USTACK_VA_END - USTACK_VA_BASE;
    assert!(
        span % USTACK_STRIDE == 0,
        "USTACK region misaligned to stride"
    );
    assert!(
        (span / USTACK_STRIDE) as usize == USTACK_MAX_SLOTS,
        "USTACK_MAX_SLOTS mismatched with region size / stride",
    );
};

// ---------------------------------------------------------------------------
// Per-region statics.
// ---------------------------------------------------------------------------

static GLOBAL: IrqMutex<StackVaAllocator<UstackRegion, BITMAP_WORDS>> =
    IrqMutex::new(StackVaAllocator::new_uninit(), LOCK_LEVEL_ALLOCATOR);

static PCP: PcpArray<UstackRegion, PCP_CAPACITY> = PcpArray::new({
    const INIT: PerCpuStackCache<UstackRegion, PCP_CAPACITY> = PerCpuStackCache::new();
    [INIT; slopos_arch::pcr::MAX_CPUS]
});

// ---------------------------------------------------------------------------
// StackRegion impl: constants + per-region wiring.
// ---------------------------------------------------------------------------

impl StackRegion for UstackRegion {
    const NAME: &'static str = "ustack";
    const VA_BASE: u64 = USTACK_VA_BASE;
    const VA_END: u64 = USTACK_VA_END;
    const STRIDE: u64 = USTACK_STRIDE;
    const GUARD_SIZE: u64 = crate::memory_layout_defs::USTACK_GUARD_SIZE;
    const MAX_SLOTS: usize = USTACK_MAX_SLOTS;
    const BITMAP_WORDS: usize = BITMAP_WORDS;
    const PCP_CAPACITY: usize = PCP_CAPACITY;
    const PCP_REFILL_BATCH: usize = PCP_REFILL_BATCH;
    const PCP_SPILL_BATCH: usize = PCP_SPILL_BATCH;
    const PAGE_FLAGS: u64 = PAGE_FLAGS;

    fn _slot_pop() -> Option<SlotEntry> {
        slot_pop_impl::<UstackRegion, BITMAP_WORDS, PCP_CAPACITY, PCP_REFILL_BATCH>(&GLOBAL, &PCP)
    }

    fn _slot_push(entry: SlotEntry) {
        slot_push_impl::<UstackRegion, BITMAP_WORDS, PCP_CAPACITY, PCP_SPILL_BATCH>(
            &GLOBAL, &PCP, entry,
        );
    }

    fn _pcp_flush_current() {
        pcp_flush_current_impl::<UstackRegion, BITMAP_WORDS, PCP_CAPACITY, PCP_SPILL_BATCH>(
            &GLOBAL, &PCP,
        );
    }

    fn _pcp_drain_all() {
        pcp_drain_all_impl::<UstackRegion, BITMAP_WORDS, PCP_CAPACITY, PCP_SPILL_BATCH>(
            &GLOBAL, &PCP,
        );
    }

    fn _pcp_stats(cpu: usize) -> PcpStats {
        PCP.snapshot(cpu)
    }

    fn _in_use_count() -> u32 {
        GLOBAL.lock().in_use()
    }

    fn _init() {
        init_with_sentinels::<UstackRegion, BITMAP_WORDS>(&GLOBAL);
    }
}

// ---------------------------------------------------------------------------
// Compatibility re-exports under the historical `ustack_va::*` names.
//
// Phases 4 and 6 will retire most of these in favour of direct
// `slopos_mm::stack_va::<UstackRegion>` calls; for now they keep
// existing callers (sched_tests, safestack_rt-adjacent code) compiling
// untouched.
// ---------------------------------------------------------------------------

/// RAII handle owning one slot in the U region.
pub type UstackSlot = StackSlot<UstackRegion>;

/// Diagnostic snapshot of one CPU's U-region cache.  Type-aliased to
/// the generic `PcpStats`; no behavioural difference.
pub type UstackPcpStats = PcpStats;

#[inline]
pub fn alloc_slot() -> Option<UstackSlot> {
    crate::stack_va::alloc_slot::<UstackRegion>()
}

#[inline]
pub fn init() {
    crate::stack_va::init::<UstackRegion>();
}

#[inline]
pub fn in_use_count() -> u32 {
    crate::stack_va::in_use_count::<UstackRegion>()
}

#[inline]
pub fn ustack_pcp_drain_all() {
    crate::stack_va::pcp_drain_all::<UstackRegion>();
}

#[inline]
pub fn ustack_pcp_flush_current() {
    crate::stack_va::pcp_flush_current::<UstackRegion>();
}

#[inline]
pub fn ustack_pcp_stats(cpu: usize) -> UstackPcpStats {
    crate::stack_va::pcp_stats::<UstackRegion>(cpu)
}

#[inline]
pub const fn pcp_capacity() -> usize {
    UstackRegion::PCP_CAPACITY
}
