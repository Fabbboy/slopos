//! Kernel-stack virtual-address allocator — thin instantiation of the
//! generic [`crate::stack_va`] core for the [`KstackRegion`] tag.
//!
//! Manages the kernel-mode task-stack VA region
//! (`KSTACK_VA_BASE..KSTACK_VA_END`) as a bitmap of fixed-stride slots,
//! each large enough to hold one task's kernel stack (including a guard
//! page).  The allocator hands out opaque [`KstackSlot`] handles that
//! name a virtual range; physical frames and PTE mappings live one
//! layer up in `core::scheduler::task_stack::TaskStack<KstackRegion>`.
//!
//! All algorithmic logic lives in [`crate::stack_va`].  This file
//! contributes:
//!
//! - The [`KstackRegion`] `StackRegion` trait impl (constants + wiring).
//! - Two private statics: the global bitmap allocator and the per-CPU
//!   cache array.  They are intentionally separate from the U-region
//!   statics so the two locks contend independently and a churn storm
//!   on one region does not stall the other.
//! - Compatibility re-exports under the historical `kstack_va::*` names
//!   so existing callers (`boot::shutdown`, scheduler tests) keep
//!   compiling unchanged.
//!
//! # Memory decoupling
//!
//! The backing region is a fixed slice of kernel virtual address space
//! declared in [`crate::memory_layout_defs`].  Growing the kernel image
//! moves `_kernel_end` but has no effect on this region — task-stack
//! capacity is therefore independent of kernel binary size.

use slopos_sync::{IrqMutex, LOCK_LEVEL_ALLOCATOR};

use crate::memory_layout_defs::{KSTACK_MAX_SLOTS, KSTACK_STRIDE, KSTACK_VA_BASE, KSTACK_VA_END};
use crate::paging_defs::PageFlags;
use crate::stack_region::{KstackRegion, StackRegion};
use crate::stack_va::{
    PcpArray, PcpStats, PerCpuStackCache, SlotEntry, StackSlot, StackVaAllocator,
    init_with_sentinels, pcp_drain_all_impl, pcp_flush_current_impl, slot_pop_impl, slot_push_impl,
};

// ---------------------------------------------------------------------------
// Region constants — the only knobs that distinguish K from U.
// ---------------------------------------------------------------------------

const BITMAP_WORDS: usize = KSTACK_MAX_SLOTS.div_ceil(64);
const PCP_CAPACITY: usize = 16;
const PCP_REFILL_BATCH: usize = 8;
const PCP_SPILL_BATCH: usize = 8;
const PAGE_FLAGS: u64 = PageFlags::KERNEL_RW.bits() | PageFlags::NO_EXECUTE.bits();

// Compile-time region-shape sanity (matches what the old hand-rolled
// allocator asserted).
const _: () = {
    let span = KSTACK_VA_END - KSTACK_VA_BASE;
    assert!(
        span % KSTACK_STRIDE == 0,
        "KSTACK region misaligned to stride"
    );
    assert!(
        (span / KSTACK_STRIDE) as usize == KSTACK_MAX_SLOTS,
        "KSTACK_MAX_SLOTS mismatched with region size / stride",
    );
};

// ---------------------------------------------------------------------------
// Per-region statics.
// ---------------------------------------------------------------------------

static GLOBAL: IrqMutex<StackVaAllocator<KstackRegion, BITMAP_WORDS>> =
    IrqMutex::new(StackVaAllocator::new_uninit(), LOCK_LEVEL_ALLOCATOR);

static PCP: PcpArray<KstackRegion, PCP_CAPACITY> = PcpArray::new({
    const INIT: PerCpuStackCache<KstackRegion, PCP_CAPACITY> = PerCpuStackCache::new();
    [INIT; slopos_arch::pcr::MAX_CPUS]
});

// ---------------------------------------------------------------------------
// StackRegion impl: constants + per-region wiring.
// ---------------------------------------------------------------------------

impl StackRegion for KstackRegion {
    const NAME: &'static str = "kstack";
    const VA_BASE: u64 = KSTACK_VA_BASE;
    const VA_END: u64 = KSTACK_VA_END;
    const STRIDE: u64 = KSTACK_STRIDE;
    const GUARD_SIZE: u64 = crate::memory_layout_defs::KSTACK_GUARD_SIZE;
    const MAX_SLOTS: usize = KSTACK_MAX_SLOTS;
    const BITMAP_WORDS: usize = BITMAP_WORDS;
    const PCP_CAPACITY: usize = PCP_CAPACITY;
    const PCP_REFILL_BATCH: usize = PCP_REFILL_BATCH;
    const PCP_SPILL_BATCH: usize = PCP_SPILL_BATCH;
    const PAGE_FLAGS: u64 = PAGE_FLAGS;

    fn _slot_pop() -> Option<SlotEntry> {
        slot_pop_impl::<KstackRegion, BITMAP_WORDS, PCP_CAPACITY, PCP_REFILL_BATCH>(&GLOBAL, &PCP)
    }

    fn _slot_push(entry: SlotEntry) {
        slot_push_impl::<KstackRegion, BITMAP_WORDS, PCP_CAPACITY, PCP_SPILL_BATCH>(
            &GLOBAL, &PCP, entry,
        );
    }

    fn _pcp_flush_current() {
        pcp_flush_current_impl::<KstackRegion, BITMAP_WORDS, PCP_CAPACITY, PCP_SPILL_BATCH>(
            &GLOBAL, &PCP,
        );
    }

    fn _pcp_drain_all() {
        pcp_drain_all_impl::<KstackRegion, BITMAP_WORDS, PCP_CAPACITY, PCP_SPILL_BATCH>(
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
        init_with_sentinels::<KstackRegion, BITMAP_WORDS>(&GLOBAL);
    }
}

// ---------------------------------------------------------------------------
// Compatibility re-exports under the historical `kstack_va::*` names.
//
// Phases 5 and 6 will retire most of these in favour of direct
// `slopos_mm::stack_va::<KstackRegion>` calls; for now they keep
// existing callers (boot::shutdown, sched_tests) compiling untouched.
// ---------------------------------------------------------------------------

/// RAII handle owning one slot in the K region.
pub type KstackSlot = StackSlot<KstackRegion>;

/// Diagnostic snapshot of one CPU's K-region cache.  Type-aliased to
/// the generic `PcpStats`; no behavioural difference.
pub type KstackPcpStats = PcpStats;

#[inline]
pub fn alloc_slot() -> Option<KstackSlot> {
    crate::stack_va::alloc_slot::<KstackRegion>()
}

#[inline]
pub fn init() {
    crate::stack_va::init::<KstackRegion>();
}

#[inline]
pub fn in_use_count() -> u32 {
    crate::stack_va::in_use_count::<KstackRegion>()
}

#[inline]
pub fn kstack_pcp_drain_all() {
    crate::stack_va::pcp_drain_all::<KstackRegion>();
}

#[inline]
pub fn kstack_pcp_flush_current() {
    crate::stack_va::pcp_flush_current::<KstackRegion>();
}

#[inline]
pub fn kstack_pcp_stats(cpu: usize) -> KstackPcpStats {
    crate::stack_va::pcp_stats::<KstackRegion>(cpu)
}

#[inline]
pub const fn pcp_capacity() -> usize {
    KstackRegion::PCP_CAPACITY
}
