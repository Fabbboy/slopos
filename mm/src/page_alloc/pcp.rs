//! Per-CPU page caches for order-0 allocations.
//!
//! Each CPU owns a stack of cached frame numbers, popped and pushed under a
//! [`PreemptGuard`] to avoid the global buddy lock. All policy (watermarks,
//! refill batching, descriptor state) lives in
//! [`super::buddy::BuddyAllocator`].

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_arch::pcr::MAX_CPUS;
use slopos_ostd::sync::cpu_local::{CacheAligned, CpuLocal};
use slopos_ostd::sync::{InitFlag, PreemptGuard};

use super::buddy::INVALID_PAGE_FRAME;

pub(super) const PCP_CAPACITY: usize = 64;
pub(super) const PCP_LOW_WATERMARK: u32 = 8;
pub(super) const PCP_HIGH_WATERMARK: u32 = PCP_CAPACITY as u32;
pub(super) const PCP_BATCH_SIZE: u32 = 16;

/// Per-CPU page cache with an array-based stack.
///
/// `count`/`stack` are non-atomic: only the owning CPU touches them, under a
/// [`PreemptGuard`]. The counters are atomic for cross-CPU stat reads.
#[repr(C, align(64))]
pub(super) struct PerCpuPageCache {
    pub(super) stack: [u32; PCP_CAPACITY],
    pub(super) count: u32,
    pub(super) alloc_count: AtomicU32,
    pub(super) free_count: AtomicU32,
}

impl PerCpuPageCache {
    const fn new() -> Self {
        Self {
            stack: [INVALID_PAGE_FRAME; PCP_CAPACITY],
            count: 0,
            alloc_count: AtomicU32::new(0),
            free_count: AtomicU32::new(0),
        }
    }
}

pub(super) static PER_CPU_CACHES: CpuLocal<PerCpuPageCache> = {
    const INIT: CacheAligned<PerCpuPageCache> = CacheAligned(PerCpuPageCache::new());
    CpuLocal::new_with([INIT; MAX_CPUS])
};

/// Gates the order-0 fast path: until set, allocations bypass the PCP and go
/// straight to the buddy free-lists.
static PCP_INIT: InitFlag = InitFlag::new();

/// Whether the order-0 fast path may consult the per-CPU cache; touching a
/// slot still needs a [`PreemptGuard`].
#[inline]
pub(super) fn is_live() -> bool {
    PCP_INIT.is_set()
}

/// One-shot flip to live, once the buddy free-lists have been populated.
pub(super) fn mark_live() {
    PCP_INIT.mark_set();
}

/// Mutable view of `cpu`'s cache. Caller holds a [`PreemptGuard`] so `cpu`
/// stays stable for the borrow's lifetime.
#[inline]
pub(super) fn cache_mut(cpu: usize) -> Option<&'static mut PerCpuPageCache> {
    debug_assert!(
        PreemptGuard::is_active(),
        "pcp::cache_mut requires PreemptGuard"
    );
    if cpu >= MAX_CPUS {
        return None;
    }
    Some(PER_CPU_CACHES.get_pinned_mut(cpu))
}

/// Snapshot stats for `cpu`'s cache; atomic loads, no pinning required.
pub(super) fn snapshot(cpu: usize) -> Option<(u32, u32, u32)> {
    if cpu >= MAX_CPUS {
        return None;
    }
    let cache = PER_CPU_CACHES.snapshot_for_cpu(cpu)?;
    Some((
        cache.count,
        cache.alloc_count.load(Ordering::Relaxed),
        cache.free_count.load(Ordering::Relaxed),
    ))
}

/// Sum cached frame counts across every CPU; global stats fold these back
/// into the "free" tally.
pub(super) fn total_cached() -> u32 {
    let mut total = 0u32;
    for cpu in 0..MAX_CPUS {
        if let Some(cache) = PER_CPU_CACHES.snapshot_for_cpu(cpu) {
            total = total.saturating_add(cache.count);
        }
    }
    total
}

/// Iterate every CPU's cache at shutdown; the buddy orchestration holds its
/// lock and drains each stack through this callback.
pub(super) fn for_each_at_shutdown(mut f: impl FnMut(usize, &mut PerCpuPageCache)) {
    PER_CPU_CACHES.for_each_mut_at_shutdown(|cpu, cache| f(cpu, cache));
}
