//! Generic fixed-stride VA-region slot allocator with a per-CPU cache.
//!
//! A `SpinLock`-protected global bitmap allocator is the source of truth; the
//! global `backed_bitmap` may lag for slots living in a CPU's cache and is
//! re-synchronised on spill. `KstackRegion` and `UstackRegion` keep separate
//! statics so their locks contend independently.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU32, Ordering};
use slopos_ostd::lock_class;

use slopos_abi::addr::VirtAddr;
use slopos_arch::pcr::MAX_CPUS;
use slopos_ostd::sync::cpu_local::{CacheAligned, CpuLocal};
use slopos_ostd::sync::{LOCK_LEVEL_ALLOCATOR, PreemptGuard, SpinLock};

use crate::kernel_mappings::kernel_map_4kb;
use crate::page_alloc::alloc_kernel_page;
use crate::paging_defs::PAGE_SIZE_2MB;
use crate::paging_defs::PageFlags;
use crate::stack_region::StackRegion;

/// One entry returned by batch alloc / consumed by batch release.  The
/// `backed` bit travels with the slot so warm reuse can skip frame mapping.
#[derive(Clone, Copy, Debug)]
pub struct SlotEntry {
    pub idx: u32,
    pub backed: bool,
}

/// Diagnostic snapshot of one CPU's cache state.
#[derive(Default, Clone, Copy)]
pub struct PcpStats {
    pub count: u32,
    pub alloc_count: u32,
    pub free_count: u32,
    pub refill_count: u32,
    pub spill_count: u32,
}

/// RAII handle owning one slot in the `R` VA region, returned to the per-CPU
/// cache on drop (or spilled to the global allocator if the cache is full).
///
/// `PhantomData<fn() -> R>` makes each region a distinct nominal type without
/// inheriting `Send`/`Sync`/`Drop` semantics from `R`.
pub struct StackSlot<R: StackRegion> {
    idx: u32,
    was_backed: bool,
    _r: PhantomData<fn() -> R>,
}

impl<R: StackRegion> StackSlot<R> {
    #[doc(hidden)]
    #[inline]
    pub fn from_entry(entry: SlotEntry) -> Self {
        Self {
            idx: entry.idx,
            was_backed: entry.backed,
            _r: PhantomData,
        }
    }

    /// Lowest VA in this slot (inclusive of the guard page).
    #[inline]
    pub fn va_base(&self) -> VirtAddr {
        VirtAddr::new(R::VA_BASE + self.idx as u64 * R::STRIDE)
    }

    /// VA one past the slot's end (exclusive).
    #[inline]
    pub fn va_end(&self) -> VirtAddr {
        VirtAddr::new(self.va_base().as_u64() + R::STRIDE)
    }

    #[inline]
    pub fn index(&self) -> u32 {
        self.idx
    }

    /// `true` if the slot had frames mapped on a prior allocation cycle; the
    /// caller may then zero the existing mapping instead of allocating one.
    #[inline]
    pub fn was_backed(&self) -> bool {
        self.was_backed
    }

    /// In-memory only — the global `backed_bitmap` syncs lazily on spill.
    #[inline]
    pub fn mark_backed(&mut self) {
        self.was_backed = true;
    }
}

impl<R: StackRegion> Drop for StackSlot<R> {
    fn drop(&mut self) {
        R::_slot_push(SlotEntry {
            idx: self.idx,
            backed: self.was_backed,
        });
    }
}

/// Global bitmap allocator for one stack region: `free_bitmap` (1 = slot is
/// free) and `backed_bitmap` (1 = slot has physical frames mapped).
pub struct StackVaAllocator<R: StackRegion, const WORDS: usize> {
    free_bitmap: [u64; WORDS],
    backed_bitmap: [u64; WORDS],
    hint: u32,
    in_use: u32,
    ready: bool,
    _r: PhantomData<fn() -> R>,
}

impl<R: StackRegion, const WORDS: usize> StackVaAllocator<R, WORDS> {
    /// `WORDS` must match `R::BITMAP_WORDS`. Referenced inside `new_uninit` so
    /// monomorphisation runs the assertion for every concrete `<R, WORDS>`.
    const _CONSISTENT: () = {
        assert!(
            WORDS == R::BITMAP_WORDS,
            "StackVaAllocator: WORDS const generic must equal R::BITMAP_WORDS",
        );
    };

    /// Empty allocator suitable for a `static`; `init()` must run once before
    /// any `alloc_one` / `release_one`.
    pub const fn new_uninit() -> Self {
        let _: () = Self::_CONSISTENT;
        Self {
            free_bitmap: [0u64; WORDS],
            backed_bitmap: [0u64; WORDS],
            hint: 0,
            in_use: 0,
            ready: false,
            _r: PhantomData,
        }
    }

    /// Mark every slot free and unbacked. Idempotent.
    pub fn init(&mut self) {
        for w in self.free_bitmap.iter_mut() {
            *w = !0u64;
        }
        for w in self.backed_bitmap.iter_mut() {
            *w = 0;
        }
        let total_bits = WORDS * 64;
        if total_bits > R::MAX_SLOTS {
            let extra = total_bits - R::MAX_SLOTS;
            let last = WORDS - 1;
            let keep = 64 - extra;
            let mask = if keep == 0 { 0 } else { !0u64 >> (64 - keep) };
            self.free_bitmap[last] &= mask;
        }
        self.hint = 0;
        self.in_use = 0;
        self.ready = true;
    }

    /// `None` if the region is exhausted or `init()` has not run.
    pub fn alloc_one(&mut self) -> Option<SlotEntry> {
        if !self.ready {
            return None;
        }
        let start_word = (self.hint as usize / 64) % WORDS;
        for offset in 0..WORDS {
            let word_idx = (start_word + offset) % WORDS;
            let word = self.free_bitmap[word_idx];
            if word == 0 {
                continue;
            }
            let bit = word.trailing_zeros() as usize;
            let slot = word_idx * 64 + bit;
            if slot >= R::MAX_SLOTS {
                continue;
            }
            let mask = 1u64 << bit;
            self.free_bitmap[word_idx] &= !mask;
            let backed = self.backed_bitmap[word_idx] & mask != 0;
            self.in_use += 1;
            self.hint = ((slot + 1) % R::MAX_SLOTS) as u32;
            return Some(SlotEntry {
                idx: slot as u32,
                backed,
            });
        }
        None
    }

    /// Returns the number of `SlotEntry`s written to the front of `out`.
    pub fn alloc_one_into_slice(&mut self, out: &mut [SlotEntry]) -> usize {
        let mut filled = 0;
        while filled < out.len() {
            match self.alloc_one() {
                Some(entry) => {
                    out[filled] = entry;
                    filled += 1;
                }
                None => break,
            }
        }
        filled
    }

    pub fn release_one(&mut self, entry: SlotEntry) {
        let slot = entry.idx as usize;
        debug_assert!(slot < R::MAX_SLOTS, "stack_va: release out of range");
        let word_idx = slot / 64;
        let bit = slot % 64;
        let mask = 1u64 << bit;
        debug_assert!(
            self.free_bitmap[word_idx] & mask == 0,
            "stack_va: double-release of slot {}",
            slot
        );
        if entry.backed {
            self.backed_bitmap[word_idx] |= mask;
        }
        self.free_bitmap[word_idx] |= mask;
        self.in_use = self.in_use.saturating_sub(1);
    }

    pub fn release_batch(&mut self, entries: &[SlotEntry]) {
        for entry in entries {
            self.release_one(*entry);
        }
    }

    /// Permanently mark a slot as allocated, removing it from the free pool.
    pub fn reserve_sentinel(&mut self, idx: u32) {
        let slot = idx as usize;
        debug_assert!(slot < R::MAX_SLOTS);
        let word_idx = slot / 64;
        let bit = slot % 64;
        let mask = 1u64 << bit;
        self.free_bitmap[word_idx] &= !mask;
        self.backed_bitmap[word_idx] |= mask;
    }

    /// Slots outside the global free pool: those cached per-CPU plus those
    /// held by live `StackSlot<R>` handles.
    #[inline]
    pub fn in_use(&self) -> u32 {
        self.in_use
    }
}

/// One CPU's stack-slot cache; `stack_idx[0..count]` and
/// `stack_backed[0..count]` are parallel arrays.  Access requires a
/// `PreemptGuard` for CPU pinning; the stat counters are readable from any CPU.
#[repr(C, align(64))]
pub struct PerCpuStackCache<R: StackRegion, const CAP: usize> {
    pub stack_idx: [u32; CAP],
    pub stack_backed: [bool; CAP],
    pub count: u32,
    pub alloc_count: AtomicU32,
    pub free_count: AtomicU32,
    pub refill_count: AtomicU32,
    pub spill_count: AtomicU32,
    _r: PhantomData<fn() -> R>,
}

impl<R: StackRegion, const CAP: usize> PerCpuStackCache<R, CAP> {
    pub const fn new() -> Self {
        Self {
            stack_idx: [u32::MAX; CAP],
            stack_backed: [false; CAP],
            count: 0,
            alloc_count: AtomicU32::new(0),
            free_count: AtomicU32::new(0),
            refill_count: AtomicU32::new(0),
            spill_count: AtomicU32::new(0),
            _r: PhantomData,
        }
    }
}

/// Per-CPU cache storage for the stack-VA allocator.
pub struct PcpArray<R: StackRegion, const CAP: usize> {
    inner: CpuLocal<PerCpuStackCache<R, CAP>>,
}

impl<R: StackRegion, const CAP: usize> PcpArray<R, CAP> {
    pub const fn new(init: [CacheAligned<PerCpuStackCache<R, CAP>>; MAX_CPUS]) -> Self {
        Self {
            inner: CpuLocal::new_with(init),
        }
    }

    /// Mutable slot access for a caller already holding a `PreemptGuard`.
    /// `cpu` must equal the current CPU.
    #[inline]
    pub fn cache(&self, cpu: usize) -> &mut PerCpuStackCache<R, CAP> {
        self.inner.get_pinned_mut(cpu)
    }

    /// Read another CPU's cache for diagnostics. Benign data race on the
    /// non-atomic fields.
    pub fn snapshot(&self, cpu: usize) -> PcpStats {
        let Some(cache) = self.inner.snapshot_for_cpu(cpu) else {
            return PcpStats::default();
        };
        PcpStats {
            count: cache.count,
            alloc_count: cache.alloc_count.load(Ordering::Relaxed),
            free_count: cache.free_count.load(Ordering::Relaxed),
            refill_count: cache.refill_count.load(Ordering::Relaxed),
            spill_count: cache.spill_count.load(Ordering::Relaxed),
        }
    }

    /// Borrow the underlying `CpuLocal` for shutdown-only drain helpers.
    #[inline]
    pub fn cpu_local(&self) -> &CpuLocal<PerCpuStackCache<R, CAP>> {
        &self.inner
    }
}

/// Refill the current CPU's cache from the global allocator in one batch.
///
/// # Safety
/// Caller must hold a `PreemptGuard`.
pub fn pcp_refill<R: StackRegion, const WORDS: usize, const CAP: usize, const REFILL: usize>(
    global: &SpinLock<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
    cpu: usize,
) {
    debug_assert!(
        PreemptGuard::is_active(),
        "stack_va::pcp_refill requires PreemptGuard"
    );
    debug_assert!(REFILL <= CAP);
    let cache = pcp.cache(cpu);
    if cache.count as usize >= REFILL {
        return;
    }

    let room = CAP - cache.count as usize;
    let want = REFILL.min(room);
    if want == 0 {
        return;
    }

    // Pre-populated with a sentinel so the batch iterates by value, avoiding
    // the `MaybeUninit` reborrow path.
    let mut batch: [SlotEntry; REFILL] = [SlotEntry {
        idx: u32::MAX,
        backed: false,
    }; REFILL];
    let got = {
        let mut alloc = global.lock();
        alloc.alloc_one_into_slice(&mut batch[..want])
    };

    for entry in batch.iter().take(got).copied() {
        let c = cache.count as usize;
        cache.stack_idx[c] = entry.idx;
        cache.stack_backed[c] = entry.backed;
        cache.count += 1;
    }
    if got > 0 {
        cache.refill_count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Spill a batch of cached slots back to the global allocator when the cache
/// overflows.
///
/// # Safety
/// Caller must hold a `PreemptGuard`.
pub fn pcp_spill<R: StackRegion, const WORDS: usize, const CAP: usize, const SPILL: usize>(
    global: &SpinLock<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
    cpu: usize,
) {
    debug_assert!(
        PreemptGuard::is_active(),
        "stack_va::pcp_spill requires PreemptGuard"
    );
    debug_assert!(SPILL < CAP);
    let cache = pcp.cache(cpu);
    let want = SPILL.min(cache.count as usize);
    if want == 0 {
        return;
    }

    // Spill the oldest entries so the hot LIFO tail stays cache-resident.
    let mut batch: [SlotEntry; SPILL] = [SlotEntry {
        idx: u32::MAX,
        backed: false,
    }; SPILL];
    for i in 0..want {
        batch[i] = SlotEntry {
            idx: cache.stack_idx[i],
            backed: cache.stack_backed[i],
        };
    }

    let remaining = cache.count as usize - want;
    for i in 0..remaining {
        cache.stack_idx[i] = cache.stack_idx[i + want];
        cache.stack_backed[i] = cache.stack_backed[i + want];
    }
    for i in remaining..cache.count as usize {
        cache.stack_idx[i] = u32::MAX;
        cache.stack_backed[i] = false;
    }
    cache.count -= want as u32;

    {
        let mut alloc = global.lock();
        alloc.release_batch(&batch[..want]);
    }
    cache.spill_count.fetch_add(1, Ordering::Relaxed);
}

/// Drain every CPU's cache back to the global allocator.  Shutdown only — no
/// concurrent PCP activity assumed.
pub fn pcp_drain_all_impl<
    R: StackRegion,
    const WORDS: usize,
    const CAP: usize,
    const SPILL: usize,
>(
    global: &SpinLock<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
) {
    pcp.cpu_local().for_each_mut_at_shutdown(|_cpu, cache| {
        while cache.count > 0 {
            let n = cache.count as usize;
            let take = SPILL.min(n);
            let mut batch: [SlotEntry; SPILL] = [SlotEntry {
                idx: u32::MAX,
                backed: false,
            }; SPILL];
            let start = n - take;
            for i in 0..take {
                batch[i] = SlotEntry {
                    idx: cache.stack_idx[start + i],
                    backed: cache.stack_backed[start + i],
                };
                cache.stack_idx[start + i] = u32::MAX;
                cache.stack_backed[start + i] = false;
            }
            cache.count -= take as u32;
            let mut alloc = global.lock();
            alloc.release_batch(&batch[..take]);
        }
    });
}

/// Drain the current CPU's cache.  Test helper: makes global-bitmap state
/// observable without chasing PCP effects.
pub fn pcp_flush_current_impl<
    R: StackRegion,
    const WORDS: usize,
    const CAP: usize,
    const SPILL: usize,
>(
    global: &SpinLock<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
) {
    let _no_migrate = PreemptGuard::new();
    let cpu = slopos_arch::pcr::get_current_cpu();
    let cache = pcp.cache(cpu);
    while cache.count > 0 {
        let n = cache.count as usize;
        let take = SPILL.min(n);
        let mut batch: [SlotEntry; SPILL] = [SlotEntry {
            idx: u32::MAX,
            backed: false,
        }; SPILL];
        let start = n - take;
        for i in 0..take {
            batch[i] = SlotEntry {
                idx: cache.stack_idx[start + i],
                backed: cache.stack_backed[start + i],
            };
            cache.stack_idx[start + i] = u32::MAX;
            cache.stack_backed[start + i] = false;
        }
        cache.count -= take as u32;
        let mut alloc = global.lock();
        alloc.release_batch(&batch[..take]);
    }
}

/// Pop from this CPU's cache, refilling from the global allocator if empty.
pub fn slot_pop_impl<R: StackRegion, const WORDS: usize, const CAP: usize, const REFILL: usize>(
    global: &SpinLock<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
) -> Option<SlotEntry> {
    let _no_migrate = PreemptGuard::new();
    let cpu = slopos_arch::pcr::get_current_cpu();
    let cache = pcp.cache(cpu);

    if cache.count == 0 {
        pcp_refill::<R, WORDS, CAP, REFILL>(global, pcp, cpu);
    }
    if cache.count == 0 {
        return None;
    }

    cache.count -= 1;
    let c = cache.count as usize;
    let idx = cache.stack_idx[c];
    let backed = cache.stack_backed[c];
    cache.stack_idx[c] = u32::MAX;
    cache.stack_backed[c] = false;
    cache.alloc_count.fetch_add(1, Ordering::Relaxed);

    Some(SlotEntry { idx, backed })
}

/// Push back to this CPU's cache, spilling to the global allocator if full.
pub fn slot_push_impl<R: StackRegion, const WORDS: usize, const CAP: usize, const SPILL: usize>(
    global: &SpinLock<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
    entry: SlotEntry,
) {
    let _no_migrate = PreemptGuard::new();
    let cpu = slopos_arch::pcr::get_current_cpu();
    let cache = pcp.cache(cpu);

    if cache.count as usize >= CAP {
        pcp_spill::<R, WORDS, CAP, SPILL>(global, pcp, cpu);
    }
    debug_assert!((cache.count as usize) < CAP);
    let c = cache.count as usize;
    cache.stack_idx[c] = entry.idx;
    cache.stack_backed[c] = entry.backed;
    cache.count += 1;
    cache.free_count.fetch_add(1, Ordering::Relaxed);
}

/// Initialise the global allocator and map one sentinel slot per 2 MB chunk;
/// the sentinel forces the intermediate page table into existence so runtime
/// allocations never race on PT creation.
///
/// Panics if the page allocator runs out of frames or a sentinel mapping fails.
pub fn init_with_sentinels<R: StackRegion, const WORDS: usize>(
    global: &SpinLock<StackVaAllocator<R, WORDS>>,
) {
    {
        let mut alloc = global.lock();
        alloc.init();
    }
    install_pt_sentinels::<R, WORDS>(global);
}

fn install_pt_sentinels<R: StackRegion, const WORDS: usize>(
    global: &SpinLock<StackVaAllocator<R, WORDS>>,
) {
    let slots_per_chunk = (PAGE_SIZE_2MB / R::STRIDE) as u32;
    let chunk_count = ((R::VA_END - R::VA_BASE) / PAGE_SIZE_2MB) as u32;
    let flags = R::PAGE_FLAGS;

    for chunk in 0..chunk_count {
        let sentinel_slot = chunk * slots_per_chunk;
        global.lock().reserve_sentinel(sentinel_slot);

        let pa = alloc_kernel_page();
        if pa.is_null() {
            panic!(
                "stack_va::{}::init: out of frames for sentinel (chunk {})",
                R::NAME,
                chunk
            );
        }
        let va = VirtAddr::new(R::VA_BASE + sentinel_slot as u64 * R::STRIDE);
        if kernel_map_4kb(va, pa, flags) != 0 {
            panic!(
                "stack_va::{}::init: failed to map sentinel at {:#x}",
                R::NAME,
                va.as_u64()
            );
        }
    }
}

/// Allocate a free slot from region `R`; `None` only when the region is
/// exhausted.
#[inline]
pub fn alloc_slot<R: StackRegion>() -> Option<StackSlot<R>> {
    R::_slot_pop().map(StackSlot::from_entry)
}

/// One-shot boot-time initialisation for region `R`.  Idempotent.
#[inline]
pub fn init<R: StackRegion>() {
    R::_init();
}

#[inline]
pub fn in_use_count<R: StackRegion>() -> u32 {
    R::_in_use_count()
}

/// Drain every CPU's cache back to the global pool.  Shutdown only.
#[inline]
pub fn pcp_drain_all<R: StackRegion>() {
    R::_pcp_drain_all();
}

/// Drain the current CPU's cache.  Test helper.
#[inline]
pub fn pcp_flush_current<R: StackRegion>() {
    R::_pcp_flush_current();
}

#[inline]
pub fn pcp_stats<R: StackRegion>(cpu: usize) -> PcpStats {
    R::_pcp_stats(cpu)
}

#[inline]
pub const fn pcp_capacity<R: StackRegion>() -> usize {
    R::PCP_CAPACITY
}

const REGION_PCP_CAPACITY: usize = 16;
const REGION_PCP_REFILL_BATCH: usize = 8;
const REGION_PCP_SPILL_BATCH: usize = 8;
const REGION_PAGE_FLAGS: u64 = PageFlags::KERNEL_RW.bits() | PageFlags::NO_EXECUTE.bits();

mod kstack {
    use super::*;
    use crate::memory_layout_defs::{
        KSTACK_GUARD_SIZE, KSTACK_MAX_SLOTS, KSTACK_STRIDE, KSTACK_VA_BASE, KSTACK_VA_END,
    };
    use crate::stack_region::KstackRegion;

    pub(super) const BITMAP_WORDS: usize = KSTACK_MAX_SLOTS.div_ceil(64);

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

    pub(super) static GLOBAL: SpinLock<StackVaAllocator<KstackRegion, BITMAP_WORDS>> =
        SpinLock::new(
            StackVaAllocator::new_uninit(),
            lock_class!("kstack.GLOBAL", LOCK_LEVEL_ALLOCATOR),
        );

    pub(super) static PCP: PcpArray<KstackRegion, REGION_PCP_CAPACITY> = PcpArray::new({
        const INIT: CacheAligned<PerCpuStackCache<KstackRegion, REGION_PCP_CAPACITY>> =
            CacheAligned(PerCpuStackCache::new());
        [INIT; MAX_CPUS]
    });

    impl StackRegion for KstackRegion {
        const NAME: &'static str = "kstack";
        const VA_BASE: u64 = KSTACK_VA_BASE;
        const VA_END: u64 = KSTACK_VA_END;
        const STRIDE: u64 = KSTACK_STRIDE;
        const GUARD_SIZE: u64 = KSTACK_GUARD_SIZE;
        const MAX_SLOTS: usize = KSTACK_MAX_SLOTS;
        const BITMAP_WORDS: usize = BITMAP_WORDS;
        const PCP_CAPACITY: usize = REGION_PCP_CAPACITY;
        const PCP_REFILL_BATCH: usize = REGION_PCP_REFILL_BATCH;
        const PCP_SPILL_BATCH: usize = REGION_PCP_SPILL_BATCH;
        const PAGE_FLAGS: u64 = REGION_PAGE_FLAGS;

        fn _slot_pop() -> Option<SlotEntry> {
            slot_pop_impl::<KstackRegion, BITMAP_WORDS, REGION_PCP_CAPACITY, REGION_PCP_REFILL_BATCH>(
                &GLOBAL, &PCP,
            )
        }

        fn _slot_push(entry: SlotEntry) {
            slot_push_impl::<KstackRegion, BITMAP_WORDS, REGION_PCP_CAPACITY, REGION_PCP_SPILL_BATCH>(
                &GLOBAL, &PCP, entry,
            );
        }

        fn _pcp_flush_current() {
            pcp_flush_current_impl::<
                KstackRegion,
                BITMAP_WORDS,
                REGION_PCP_CAPACITY,
                REGION_PCP_SPILL_BATCH,
            >(&GLOBAL, &PCP);
        }

        fn _pcp_drain_all() {
            pcp_drain_all_impl::<
                KstackRegion,
                BITMAP_WORDS,
                REGION_PCP_CAPACITY,
                REGION_PCP_SPILL_BATCH,
            >(&GLOBAL, &PCP);
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
}

mod ustack {
    use super::*;
    use crate::memory_layout_defs::{
        USTACK_GUARD_SIZE, USTACK_MAX_SLOTS, USTACK_STRIDE, USTACK_VA_BASE, USTACK_VA_END,
    };
    use crate::stack_region::UstackRegion;

    pub(super) const BITMAP_WORDS: usize = USTACK_MAX_SLOTS.div_ceil(64);

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

    pub(super) static GLOBAL: SpinLock<StackVaAllocator<UstackRegion, BITMAP_WORDS>> =
        SpinLock::new(
            StackVaAllocator::new_uninit(),
            lock_class!("ustack.GLOBAL", LOCK_LEVEL_ALLOCATOR),
        );

    pub(super) static PCP: PcpArray<UstackRegion, REGION_PCP_CAPACITY> = PcpArray::new({
        const INIT: CacheAligned<PerCpuStackCache<UstackRegion, REGION_PCP_CAPACITY>> =
            CacheAligned(PerCpuStackCache::new());
        [INIT; MAX_CPUS]
    });

    impl StackRegion for UstackRegion {
        const NAME: &'static str = "ustack";
        const VA_BASE: u64 = USTACK_VA_BASE;
        const VA_END: u64 = USTACK_VA_END;
        const STRIDE: u64 = USTACK_STRIDE;
        const GUARD_SIZE: u64 = USTACK_GUARD_SIZE;
        const MAX_SLOTS: usize = USTACK_MAX_SLOTS;
        const BITMAP_WORDS: usize = BITMAP_WORDS;
        const PCP_CAPACITY: usize = REGION_PCP_CAPACITY;
        const PCP_REFILL_BATCH: usize = REGION_PCP_REFILL_BATCH;
        const PCP_SPILL_BATCH: usize = REGION_PCP_SPILL_BATCH;
        const PAGE_FLAGS: u64 = REGION_PAGE_FLAGS;

        fn _slot_pop() -> Option<SlotEntry> {
            slot_pop_impl::<UstackRegion, BITMAP_WORDS, REGION_PCP_CAPACITY, REGION_PCP_REFILL_BATCH>(
                &GLOBAL, &PCP,
            )
        }

        fn _slot_push(entry: SlotEntry) {
            slot_push_impl::<UstackRegion, BITMAP_WORDS, REGION_PCP_CAPACITY, REGION_PCP_SPILL_BATCH>(
                &GLOBAL, &PCP, entry,
            );
        }

        fn _pcp_flush_current() {
            pcp_flush_current_impl::<
                UstackRegion,
                BITMAP_WORDS,
                REGION_PCP_CAPACITY,
                REGION_PCP_SPILL_BATCH,
            >(&GLOBAL, &PCP);
        }

        fn _pcp_drain_all() {
            pcp_drain_all_impl::<
                UstackRegion,
                BITMAP_WORDS,
                REGION_PCP_CAPACITY,
                REGION_PCP_SPILL_BATCH,
            >(&GLOBAL, &PCP);
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
}
