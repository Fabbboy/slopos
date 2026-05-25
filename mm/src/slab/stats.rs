//! Aggregate heap statistics — the surface that test callers and
//! diagnostic printers consume.
//!
//! `HeapStats` is `repr(C)` so the C-ABI-style
//! `get_heap_stats(*mut HeapStats)` shim in `compat.rs` can hand off
//! a snapshot to FFI-shaped callers. Safe-fn callers should prefer
//! `get_heap_stats_owned()`.

use core::sync::atomic::Ordering;

use super::KERNEL_SLAB;
use super::SIZE_CLASSES;
use super::allocator::SlabClassStats;
use super::page::{LARGE_ALLOC_COUNT, SLAB_PAGE_COUNT};
use crate::paging_defs::PAGE_SIZE_4KB;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct HeapStats {
    pub total_size: u64,
    pub allocated_size: u64,
    pub free_size: u64,
    pub total_blocks: u32,
    pub allocated_blocks: u32,
    pub free_blocks: u32,
    pub allocation_count: u32,
    pub free_count: u32,
}

/// Collect a snapshot. Reads counters under `Relaxed` ordering — the
/// snapshot is approximate (writers may be racing) but every field
/// is internally consistent (the writer side updates each counter
/// once per operation; cross-counter skew is at most a few in-flight
/// allocations).
pub fn snapshot() -> HeapStats {
    let mut alloc_count: u64 = 0;
    let mut free_count: u64 = 0;
    let mut total_objects: u64 = 0;
    let mut free_objects: u64 = 0;
    let mut slab_allocated_bytes: u64 = 0;

    // Fold the eight per-class counters with a single loop body rather than
    // eight inlined macro expansions: in a debug build the per-expansion
    // temporaries are not coalesced, which pushed this frame past the 2 KiB
    // stack-frame ceiling that check_stack_sizes.sh enforces. One reused loop
    // body keeps the frame tiny. The slabs are distinct const-generic types but
    // share a `SlabClassStats`, so an array of `&SlabClassStats` is uniform.
    let class_stats: [&SlabClassStats; SIZE_CLASSES.len()] = [
        &KERNEL_SLAB.slab16.stats,
        &KERNEL_SLAB.slab32.stats,
        &KERNEL_SLAB.slab64.stats,
        &KERNEL_SLAB.slab128.stats,
        &KERNEL_SLAB.slab256.stats,
        &KERNEL_SLAB.slab512.stats,
        &KERNEL_SLAB.slab1024.stats,
        &KERNEL_SLAB.slab2048.stats,
    ];
    let mut idx = 0;
    while idx < class_stats.len() {
        let stats = class_stats[idx];
        let ac = stats.alloc_count.load(Ordering::Relaxed);
        let fc = stats.free_count.load(Ordering::Relaxed);
        let total = stats.total_objects.load(Ordering::Relaxed) as u64;
        let free = stats.free_objects.load(Ordering::Relaxed) as u64;
        alloc_count = alloc_count.saturating_add(ac);
        free_count = free_count.saturating_add(fc);
        total_objects = total_objects.saturating_add(total);
        free_objects = free_objects.saturating_add(free);
        let in_use = total.saturating_sub(free);
        let class_bytes = SIZE_CLASSES[idx] as u64;
        slab_allocated_bytes =
            slab_allocated_bytes.saturating_add(in_use.saturating_mul(class_bytes));
        idx += 1;
    }

    let slab_pages = SLAB_PAGE_COUNT.load(Ordering::Relaxed) as u64;
    let large_count = LARGE_ALLOC_COUNT.load(Ordering::Relaxed) as u64;
    let large_bytes = KERNEL_SLAB
        .large
        .total_bytes_allocated
        .load(Ordering::Relaxed)
        .saturating_sub(KERNEL_SLAB.large.total_bytes_freed.load(Ordering::Relaxed));

    let total_size = slab_pages
        .saturating_mul(PAGE_SIZE_4KB)
        .saturating_add(large_bytes);
    let allocated_size = slab_allocated_bytes.saturating_add(large_bytes);
    let free_size = total_size.saturating_sub(allocated_size);

    HeapStats {
        total_size,
        allocated_size,
        free_size,
        total_blocks: total_objects as u32,
        allocated_blocks: (total_objects.saturating_sub(free_objects)) as u32,
        free_blocks: free_objects as u32,
        allocation_count: alloc_count.saturating_add(large_count) as u32,
        free_count: free_count as u32,
    }
}
