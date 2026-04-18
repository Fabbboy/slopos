//! Kernel stack virtual-address allocator.
//!
//! Manages the `KSTACK_VA_BASE..KSTACK_VA_END` region as a bitmap of
//! fixed-stride slots, each large enough to hold one task's kernel stack
//! (including a guard page).  This layer deals **only** with virtual
//! address ranges — physical frames and PTE mapping live in the
//! `KernelStack` type one layer up (`core::scheduler::stack`).
//!
//! # Two-tier structure
//!
//! - **Global bitmap** (`KstackVaAllocator`, protected by `IrqMutex`) is
//!   the source of truth for slot state across the whole system.
//! - **Per-CPU cache** (`PerCpuKstackCache`, protected by `PreemptGuard`
//!   only) sits in front of the global and absorbs the warm path so
//!   cache-hitting alloc/free never touches the global lock.
//!
//! Each cached entry carries its own `was_backed` bit so the caller sees
//! the fast-reuse path for slots that already have mapped frames.  The
//! global `backed_bitmap` is allowed to lag for slots that live entirely
//! in the PCP; it gets re-synchronised on spill so a future refill picks
//! up the correct state.
//!
//! # Memory decoupling
//!
//! The backing region is a fixed slice of kernel virtual address space
//! declared in `memory_layout_defs`.  Growing the kernel image moves
//! `_kernel_end` but has no effect on this region — task-stack capacity
//! is therefore independent of kernel binary size.
//!
//! # Safety
//!
//! The VA allocator is 100% safe Rust.  It does not touch page tables
//! or physical memory; it only hands out opaque [`KstackSlot`] handles
//! that name a virtual range within the region.  The per-CPU cache
//! relies on `PreemptGuard` CPU pinning for the single `unsafe`
//! dereference of the `UnsafeCell`-wrapped cache array.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::VirtAddr;
use slopos_arch::pcr::MAX_CPUS;
use slopos_sync::{IrqMutex, LOCK_LEVEL_ALLOCATOR, PreemptGuard};

use crate::memory_layout_defs::{KSTACK_MAX_SLOTS, KSTACK_STRIDE, KSTACK_VA_BASE, KSTACK_VA_END};
use crate::page_alloc::alloc_page_frame;
use crate::paging::map_page_4kb;
use crate::paging_defs::{PAGE_SIZE_2MB, PageFlags};

/// Words of `u64` needed to cover `KSTACK_MAX_SLOTS` bits.
const BITMAP_WORDS: usize = (KSTACK_MAX_SLOTS + 63) / 64;

/// Compile-time sanity: the region must be a whole multiple of the stride.
const _: () = {
    let span = crate::memory_layout_defs::KSTACK_VA_END - KSTACK_VA_BASE;
    assert!(
        span % KSTACK_STRIDE == 0,
        "KSTACK region misaligned to stride"
    );
    assert!(
        (span / KSTACK_STRIDE) as usize == KSTACK_MAX_SLOTS,
        "KSTACK_MAX_SLOTS mismatched with region size / stride",
    );
};

/// Entry returned by batch alloc / consumed by batch release.
#[derive(Clone, Copy)]
struct SlotEntry {
    idx: u32,
    backed: bool,
}

// ---------------------------------------------------------------------------
// Global bitmap-backed allocator.
// ---------------------------------------------------------------------------

struct KstackVaAllocator {
    free_bitmap: [u64; BITMAP_WORDS],
    backed_bitmap: [u64; BITMAP_WORDS],
    hint: u32,
    in_use: u32,
    ready: bool,
}

impl KstackVaAllocator {
    const fn new_uninit() -> Self {
        Self {
            free_bitmap: [0u64; BITMAP_WORDS],
            backed_bitmap: [0u64; BITMAP_WORDS],
            hint: 0,
            in_use: 0,
            ready: false,
        }
    }

    fn init(&mut self) {
        for w in self.free_bitmap.iter_mut() {
            *w = !0u64;
        }
        for w in self.backed_bitmap.iter_mut() {
            *w = 0;
        }
        let total_bits = BITMAP_WORDS * 64;
        if total_bits > KSTACK_MAX_SLOTS {
            let extra = total_bits - KSTACK_MAX_SLOTS;
            let last = BITMAP_WORDS - 1;
            let keep = 64 - extra;
            let mask = if keep == 0 { 0 } else { !0u64 >> (64 - keep) };
            self.free_bitmap[last] &= mask;
        }
        self.hint = 0;
        self.in_use = 0;
        self.ready = true;
    }

    /// Claim one free slot, reading its current backed state.
    fn alloc_one(&mut self) -> Option<SlotEntry> {
        if !self.ready {
            return None;
        }
        let start_word = (self.hint as usize / 64) % BITMAP_WORDS;
        for offset in 0..BITMAP_WORDS {
            let word_idx = (start_word + offset) % BITMAP_WORDS;
            let word = self.free_bitmap[word_idx];
            if word == 0 {
                continue;
            }
            let bit = word.trailing_zeros() as usize;
            let slot = word_idx * 64 + bit;
            if slot >= KSTACK_MAX_SLOTS {
                continue;
            }
            let mask = 1u64 << bit;
            self.free_bitmap[word_idx] &= !mask;
            let backed = self.backed_bitmap[word_idx] & mask != 0;
            self.in_use += 1;
            self.hint = ((slot + 1) % KSTACK_MAX_SLOTS) as u32;
            return Some(SlotEntry {
                idx: slot as u32,
                backed,
            });
        }
        None
    }

    /// Claim up to `out.len()` free slots under a single lock acquisition.
    /// Returns the number of `SlotEntry`s written to the front of `out`.
    fn alloc_batch(&mut self, out: &mut [MaybeUninit<SlotEntry>]) -> usize {
        let mut filled = 0;
        while filled < out.len() {
            match self.alloc_one() {
                Some(entry) => {
                    out[filled].write(entry);
                    filled += 1;
                }
                None => break,
            }
        }
        filled
    }

    /// Release one slot, recording whether it is currently backed.
    fn release_one(&mut self, entry: SlotEntry) {
        let slot = entry.idx as usize;
        debug_assert!(slot < KSTACK_MAX_SLOTS, "kstack_va: release out of range");
        let word_idx = slot / 64;
        let bit = slot % 64;
        let mask = 1u64 << bit;
        debug_assert!(
            self.free_bitmap[word_idx] & mask == 0,
            "kstack_va: double-release of slot {}",
            slot
        );
        if entry.backed {
            self.backed_bitmap[word_idx] |= mask;
        }
        self.free_bitmap[word_idx] |= mask;
        self.in_use = self.in_use.saturating_sub(1);
    }

    /// Release a batch of slots under a single lock acquisition.
    fn release_batch(&mut self, entries: &[SlotEntry]) {
        for entry in entries {
            self.release_one(*entry);
        }
    }

    /// Permanently mark a slot as allocated (removes it from the free
    /// pool forever).  Used at init to reserve one sentinel slot per
    /// 2 MB PD chunk — the sentinel's mapping forces the intermediate
    /// page table into existence so runtime allocations never race on
    /// PT creation.
    fn reserve_sentinel(&mut self, idx: u32) {
        let slot = idx as usize;
        debug_assert!(slot < KSTACK_MAX_SLOTS);
        let word_idx = slot / 64;
        let bit = slot % 64;
        let mask = 1u64 << bit;
        self.free_bitmap[word_idx] &= !mask;
        self.backed_bitmap[word_idx] |= mask;
    }
}

static KSTACK_VA_ALLOCATOR: IrqMutex<KstackVaAllocator> =
    IrqMutex::new(KstackVaAllocator::new_uninit(), LOCK_LEVEL_ALLOCATOR);

// ---------------------------------------------------------------------------
// Per-CPU kstack-slot cache.
// ---------------------------------------------------------------------------

/// Maximum entries per CPU.  Tuned low — kstack alloc is vastly less
/// frequent than frame alloc, and each cached entry reserves one slot
/// of VA pressure (up to 64 KB of VA stride, though no physical memory
/// if the slot is unbacked).
const KSTACK_PCP_CAPACITY: usize = 16;
/// Slots drained from global per refill.
const KSTACK_PCP_REFILL_BATCH: usize = 8;
/// Slots spilled back to global when the cache overflows.  Must be
/// strictly less than `KSTACK_PCP_CAPACITY` so a subsequent push cannot
/// immediately overflow again.
const KSTACK_PCP_SPILL_BATCH: usize = 8;

/// Per-CPU cache of pre-claimed kstack slots.
///
/// `stack_idx[0..count]` and `stack_backed[0..count]` hold parallel
/// entries.  Access is guarded by `PreemptGuard` — the owning CPU has
/// exclusive mutable access while pinned.  `alloc_count` / `free_count` /
/// `refill_count` / `spill_count` are atomics so other CPUs can read
/// them (diagnostics / tests) without pinning.
#[repr(C, align(64))]
struct PerCpuKstackCache {
    stack_idx: [u32; KSTACK_PCP_CAPACITY],
    stack_backed: [bool; KSTACK_PCP_CAPACITY],
    count: u32,
    alloc_count: AtomicU32,
    free_count: AtomicU32,
    refill_count: AtomicU32,
    spill_count: AtomicU32,
}

impl PerCpuKstackCache {
    const fn new() -> Self {
        Self {
            stack_idx: [u32::MAX; KSTACK_PCP_CAPACITY],
            stack_backed: [false; KSTACK_PCP_CAPACITY],
            count: 0,
            alloc_count: AtomicU32::new(0),
            free_count: AtomicU32::new(0),
            refill_count: AtomicU32::new(0),
            spill_count: AtomicU32::new(0),
        }
    }
}

/// Wrapper providing `Sync` for the per-CPU caches.
///
/// SAFETY: Each CPU only accesses its own slot with preemption disabled.
/// Cross-CPU access is limited to reading atomic stat counters.
struct PcpArray(UnsafeCell<[PerCpuKstackCache; MAX_CPUS]>);
unsafe impl Sync for PcpArray {}

static PER_CPU_CACHES: PcpArray = {
    const INIT: PerCpuKstackCache = PerCpuKstackCache::new();
    PcpArray(UnsafeCell::new([INIT; MAX_CPUS]))
};

#[inline]
fn get_current_cpu() -> usize {
    slopos_arch::pcr::get_current_cpu()
}

/// # Safety
/// Caller must hold a `PreemptGuard` so `cpu` stays stable for the
/// lifetime of the returned reference.
#[inline]
unsafe fn pcp_cache(cpu: usize) -> &'static mut PerCpuKstackCache {
    debug_assert!(
        PreemptGuard::is_active(),
        "kstack pcp_cache requires PreemptGuard"
    );
    debug_assert!(cpu < MAX_CPUS);
    unsafe { &mut (*PER_CPU_CACHES.0.get())[cpu] }
}

/// Refill the current CPU's cache from the global allocator.  Runs with
/// one lock acquisition for the batch.
///
/// # Safety
/// Caller must hold a `PreemptGuard`.
fn kstack_pcp_refill(cpu: usize) {
    debug_assert!(
        PreemptGuard::is_active(),
        "kstack_pcp_refill requires PreemptGuard"
    );

    let cache = unsafe { pcp_cache(cpu) };
    if cache.count as usize >= KSTACK_PCP_REFILL_BATCH {
        return;
    }

    let room = KSTACK_PCP_CAPACITY - cache.count as usize;
    let want = KSTACK_PCP_REFILL_BATCH.min(room);
    if want == 0 {
        return;
    }

    let mut batch: [MaybeUninit<SlotEntry>; KSTACK_PCP_REFILL_BATCH] =
        [const { MaybeUninit::uninit() }; KSTACK_PCP_REFILL_BATCH];
    let got = {
        let mut alloc = KSTACK_VA_ALLOCATOR.lock();
        alloc.alloc_batch(&mut batch[..want])
    };

    for entry_slot in batch.iter().take(got) {
        // SAFETY: alloc_batch wrote exactly `got` entries.
        let entry = unsafe { entry_slot.assume_init() };
        let c = cache.count as usize;
        cache.stack_idx[c] = entry.idx;
        cache.stack_backed[c] = entry.backed;
        cache.count += 1;
    }
    if got > 0 {
        cache.refill_count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Spill a batch of cached slots back to the global allocator.  Invoked
/// when the cache overflows.  Runs with one lock acquisition.
///
/// # Safety
/// Caller must hold a `PreemptGuard`.
fn kstack_pcp_spill(cpu: usize) {
    debug_assert!(
        PreemptGuard::is_active(),
        "kstack_pcp_spill requires PreemptGuard"
    );

    let cache = unsafe { pcp_cache(cpu) };
    let want = KSTACK_PCP_SPILL_BATCH.min(cache.count as usize);
    if want == 0 {
        return;
    }

    // Spill from the bottom of the stack (oldest) so the hot tail stays
    // cache-resident for reuse.  This preserves LIFO ordering on the
    // remaining entries at the cost of one memmove per spill.
    let mut batch: [SlotEntry; KSTACK_PCP_SPILL_BATCH] = [SlotEntry {
        idx: u32::MAX,
        backed: false,
    }; KSTACK_PCP_SPILL_BATCH];
    for i in 0..want {
        batch[i] = SlotEntry {
            idx: cache.stack_idx[i],
            backed: cache.stack_backed[i],
        };
    }

    // Compact the cache by shifting surviving entries down.
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
        let mut alloc = KSTACK_VA_ALLOCATOR.lock();
        alloc.release_batch(&batch[..want]);
    }
    cache.spill_count.fetch_add(1, Ordering::Relaxed);
}

/// Drain every CPU's cache back to the global allocator.  Intended for
/// shutdown only — no concurrent PCP activity is assumed.
pub fn kstack_pcp_drain_all() {
    for cpu in 0..MAX_CPUS {
        // SAFETY: shutdown-only — no concurrent access.
        let cache = unsafe { &mut (*PER_CPU_CACHES.0.get())[cpu] };
        while cache.count > 0 {
            let n = cache.count as usize;
            let take = KSTACK_PCP_SPILL_BATCH.min(n);
            let mut batch: [SlotEntry; KSTACK_PCP_SPILL_BATCH] = [SlotEntry {
                idx: u32::MAX,
                backed: false,
            };
                KSTACK_PCP_SPILL_BATCH];
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
            let mut alloc = KSTACK_VA_ALLOCATOR.lock();
            alloc.release_batch(&batch[..take]);
        }
    }
}

/// Diagnostic snapshot of one CPU's cache state.  Used by tests and
/// `kstack_pcp_stats_total()`.
#[derive(Default, Clone, Copy)]
pub struct KstackPcpStats {
    pub count: u32,
    pub alloc_count: u32,
    pub free_count: u32,
    pub refill_count: u32,
    pub spill_count: u32,
}

pub fn kstack_pcp_stats(cpu: usize) -> KstackPcpStats {
    if cpu >= MAX_CPUS {
        return KstackPcpStats::default();
    }
    // SAFETY: reading another CPU's cache fields is a benign race for
    // diagnostics.  The atomics enforce Ordering::Relaxed visibility.
    let cache = unsafe { &(*PER_CPU_CACHES.0.get())[cpu] };
    KstackPcpStats {
        count: cache.count,
        alloc_count: cache.alloc_count.load(Ordering::Relaxed),
        free_count: cache.free_count.load(Ordering::Relaxed),
        refill_count: cache.refill_count.load(Ordering::Relaxed),
        spill_count: cache.spill_count.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// Public RAII handle.
// ---------------------------------------------------------------------------

/// RAII handle owning one slot in the kernel-stack VA region.
///
/// The slot is returned to the per-CPU cache (or spilled to the global
/// allocator if the cache is full) automatically on drop.  The handle
/// itself does not map any physical memory — that is handled by
/// `core::scheduler::stack::KernelStack`.
///
/// `KstackSlot` is deliberately **not** `Clone`/`Copy` so double-release
/// is impossible by construction.
pub struct KstackSlot {
    idx: u32,
    was_backed: bool,
}

impl KstackSlot {
    #[inline]
    pub fn va_base(&self) -> VirtAddr {
        VirtAddr::new(KSTACK_VA_BASE + self.idx as u64 * KSTACK_STRIDE)
    }

    #[inline]
    pub fn va_end(&self) -> VirtAddr {
        VirtAddr::new(self.va_base().as_u64() + KSTACK_STRIDE)
    }

    #[inline]
    pub fn index(&self) -> u32 {
        self.idx
    }

    #[inline]
    pub fn was_backed(&self) -> bool {
        self.was_backed
    }

    /// Record that the caller has mapped physical frames into this
    /// slot's VA range.  In-memory only — the global `backed_bitmap`
    /// syncs lazily on spill.  Idempotent.
    pub fn mark_backed(&mut self) {
        self.was_backed = true;
    }
}

impl Drop for KstackSlot {
    fn drop(&mut self) {
        let _no_migrate = PreemptGuard::new();
        let cpu = get_current_cpu();
        // SAFETY: PreemptGuard pins us to this CPU.
        let cache = unsafe { pcp_cache(cpu) };

        if cache.count as usize >= KSTACK_PCP_CAPACITY {
            kstack_pcp_spill(cpu);
        }

        debug_assert!((cache.count as usize) < KSTACK_PCP_CAPACITY);
        let c = cache.count as usize;
        cache.stack_idx[c] = self.idx;
        cache.stack_backed[c] = self.was_backed;
        cache.count += 1;
        cache.free_count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Allocate a free slot.
///
/// Fast path: pops from the per-CPU cache under a single `PreemptGuard`
/// (lock-free).  Slow path: refills the cache from the global allocator
/// with one `IrqMutex` acquisition.  Returns `None` only when the whole
/// KSTACK region is genuinely exhausted.
pub fn alloc_slot() -> Option<KstackSlot> {
    let _no_migrate = PreemptGuard::new();
    let cpu = get_current_cpu();
    // SAFETY: PreemptGuard pins us to this CPU.
    let cache = unsafe { pcp_cache(cpu) };

    if cache.count == 0 {
        kstack_pcp_refill(cpu);
    }

    if cache.count == 0 {
        return None;
    }

    cache.count -= 1;
    let c = cache.count as usize;
    let idx = cache.stack_idx[c];
    let was_backed = cache.stack_backed[c];
    cache.stack_idx[c] = u32::MAX;
    cache.stack_backed[c] = false;
    cache.alloc_count.fetch_add(1, Ordering::Relaxed);

    Some(KstackSlot { idx, was_backed })
}

// ---------------------------------------------------------------------------
// Init / sentinels.
// ---------------------------------------------------------------------------

pub fn init() {
    {
        let mut alloc = KSTACK_VA_ALLOCATOR.lock();
        alloc.init();
    }
    install_pt_sentinels();
}

fn install_pt_sentinels() {
    let slots_per_chunk = (PAGE_SIZE_2MB / KSTACK_STRIDE) as u32;
    let chunk_count = ((KSTACK_VA_END - KSTACK_VA_BASE) / PAGE_SIZE_2MB) as u32;

    let flags = (PageFlags::KERNEL_RW | PageFlags::NO_EXECUTE).bits();

    for chunk in 0..chunk_count {
        let sentinel_slot = chunk * slots_per_chunk;
        KSTACK_VA_ALLOCATOR.lock().reserve_sentinel(sentinel_slot);

        let pa = alloc_page_frame(0);
        if pa.is_null() {
            panic!(
                "kstack_va::init: out of frames for sentinel (chunk {})",
                chunk
            );
        }
        let va = VirtAddr::new(KSTACK_VA_BASE + sentinel_slot as u64 * KSTACK_STRIDE);
        if map_page_4kb(va, pa, flags) != 0 {
            panic!(
                "kstack_va::init: failed to map sentinel at {:#x}",
                va.as_u64()
            );
        }
    }
}

/// Number of slots currently allocated (global view — includes slots
/// held in every CPU's PCP).
pub fn in_use_count() -> u32 {
    KSTACK_VA_ALLOCATOR.lock().in_use
}

/// Test-only: forcibly return every PCP-held slot on the current CPU to
/// the global allocator.  Lets tests reason about global bitmap state
/// without chasing PCP effects.
pub fn kstack_pcp_flush_current() {
    let _no_migrate = PreemptGuard::new();
    let cpu = get_current_cpu();
    // SAFETY: PreemptGuard pins us.
    let cache = unsafe { pcp_cache(cpu) };
    while cache.count > 0 {
        let n = cache.count as usize;
        let take = KSTACK_PCP_SPILL_BATCH.min(n);
        let mut batch: [SlotEntry; KSTACK_PCP_SPILL_BATCH] = [SlotEntry {
            idx: u32::MAX,
            backed: false,
        }; KSTACK_PCP_SPILL_BATCH];
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
        let mut alloc = KSTACK_VA_ALLOCATOR.lock();
        alloc.release_batch(&batch[..take]);
    }
}

/// Test-only: per-CPU cache capacity (callers need this to force refill
/// / spill without hard-coding the constant).
pub const fn pcp_capacity() -> usize {
    KSTACK_PCP_CAPACITY
}
