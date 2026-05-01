//! Generic fixed-stride VA-region slot allocator with a per-CPU cache.
//!
//! One implementation, parameterised over [`StackRegion`].  Replaces
//! the historical `kstack_va.rs` and `ustack_va.rs` mirror modules.
//!
//! # Two-tier structure
//!
//! - **Global allocator** ([`StackVaAllocator<R, WORDS>`], protected by
//!   `IrqMutex` at `LOCK_LEVEL_ALLOCATOR`) is the source of truth for
//!   slot state across the whole system.
//! - **Per-CPU cache** ([`PerCpuStackCache<R, CAP>`], protected by
//!   `PreemptGuard` only) sits in front of the global and absorbs the
//!   warm path so a cache hit never touches the global lock.
//!
//! The cache carries a `was_backed` bit per slot so the caller sees the
//! fast-reuse path for slots that already have mapped frames.  The
//! global `backed_bitmap` is allowed to lag for slots that live entirely
//! in a CPU's cache; it gets re-synchronised on spill so a future refill
//! picks up the correct state.
//!
//! # Per-region wiring
//!
//! Each region's instantiation module owns its own statics and provides
//! the per-region wiring via the [`StackRegion`] trait's hidden `_*`
//! methods.  The public free functions in this module ([`alloc_slot`],
//! [`init`], [`pcp_drain_all`], …) are thin generic wrappers over those
//! trait methods.
//!
//! # Safety boundaries
//!
//! The VA allocator itself is 100% safe Rust: it manipulates bitmap
//! words and slot indices and never dereferences a stack pointer.  The
//! page-table sentinel install touches `mm::paging::map_page_4kb`,
//! which is a safe wrapper.  The per-CPU cache uses `UnsafeCell` and
//! relies on `PreemptGuard` CPU pinning for its single `unsafe`
//! dereference per access.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::VirtAddr;
use slopos_arch::pcr::MAX_CPUS;
use slopos_sync::{IrqMutex, PreemptGuard};

use crate::page_alloc::alloc_page_frame;
use crate::paging::map_page_4kb;
use crate::paging_defs::PAGE_SIZE_2MB;
use crate::stack_region::StackRegion;

// ---------------------------------------------------------------------------
// Cross-cutting small types.
// ---------------------------------------------------------------------------

/// One entry returned by batch alloc / consumed by batch release.  The
/// `backed` bit travels with the slot so the warm-reuse fast path can
/// skip the frame-mapping work.
#[derive(Clone, Copy, Debug)]
pub struct SlotEntry {
    pub idx: u32,
    pub backed: bool,
}

/// Diagnostic snapshot of one CPU's cache state.  Consumed by tests and
/// by the per-region `pcp_stats` shim functions.
#[derive(Default, Clone, Copy)]
pub struct PcpStats {
    pub count: u32,
    pub alloc_count: u32,
    pub free_count: u32,
    pub refill_count: u32,
    pub spill_count: u32,
}

// ---------------------------------------------------------------------------
// Public RAII slot handle.
// ---------------------------------------------------------------------------

/// RAII handle owning one slot in the `R` VA region.
///
/// Returned to the per-CPU cache on drop (or spilled to the global
/// allocator if the cache is full).  Not `Copy`/`Clone` — double-release
/// is impossible by construction.
///
/// `PhantomData<fn() -> R>` carries the region type without inheriting
/// any `Send`/`Sync`/`Drop` semantics from `R` (which is uninhabited
/// anyway).  This is what makes a `StackSlot<KstackRegion>` and a
/// `StackSlot<UstackRegion>` distinct nominal types — the compiler
/// rejects passing one where the other is expected, at zero runtime
/// cost.
pub struct StackSlot<R: StackRegion> {
    idx: u32,
    was_backed: bool,
    _r: PhantomData<fn() -> R>,
}

impl<R: StackRegion> StackSlot<R> {
    /// Construct a handle from a raw `(idx, backed)` pair.  Used only
    /// by per-region wiring after popping from the cache.
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

    /// Slot index within the region.
    #[inline]
    pub fn index(&self) -> u32 {
        self.idx
    }

    /// `true` if the slot had physical frames mapped on a prior
    /// allocation cycle.  When true, the caller's `allocate()` can skip
    /// the frame allocation + page mapping path and just zero the
    /// existing mapping.
    #[inline]
    pub fn was_backed(&self) -> bool {
        self.was_backed
    }

    /// Record that the caller has mapped physical frames into this
    /// slot's VA range.  In-memory only — the global `backed_bitmap`
    /// syncs lazily on spill.  Idempotent.
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

// ---------------------------------------------------------------------------
// Global bitmap-backed allocator (one per region).
// ---------------------------------------------------------------------------

/// Global bitmap allocator for one stack region.  Two bitmaps:
/// `free_bitmap` (1 = slot is free) and `backed_bitmap` (1 = slot has
/// physical frames mapped).  `WORDS` is the number of `u64` words —
/// must equal `R::BITMAP_WORDS`; a `const _` cross-check inside the
/// `impl` enforces this at compile time.
#[allow(dead_code)]
pub struct StackVaAllocator<R: StackRegion, const WORDS: usize> {
    free_bitmap: [u64; WORDS],
    backed_bitmap: [u64; WORDS],
    hint: u32,
    in_use: u32,
    ready: bool,
    _r: PhantomData<fn() -> R>,
}

#[allow(dead_code)]
impl<R: StackRegion, const WORDS: usize> StackVaAllocator<R, WORDS> {
    /// Compile-time consistency check: `WORDS` must match `R::BITMAP_WORDS`.
    /// Referenced inside `new_uninit` so monomorphisation forces the
    /// assertion to run for every concrete `<R, WORDS>` pair.
    const _CONSISTENT: () = {
        assert!(
            WORDS == R::BITMAP_WORDS,
            "StackVaAllocator: WORDS const generic must equal R::BITMAP_WORDS",
        );
    };

    /// Construct an empty allocator suitable for storing in a `static`.
    /// `init()` must be called once before any `alloc_one` / `release_one`.
    pub const fn new_uninit() -> Self {
        // Touch _CONSISTENT to force the assertion at instantiation.
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

    /// Mark every slot in `[0, R::MAX_SLOTS)` as free, every slot as
    /// unbacked.  Idempotent.
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

    /// Claim one free slot, reading its current backed state.  Returns
    /// `None` if the region is exhausted or the allocator is uninitialised.
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

    /// Claim up to `out.len()` free slots under a single lock acquisition.
    /// Returns the number of `SlotEntry`s written to the front of `out`.
    pub fn alloc_batch(&mut self, out: &mut [MaybeUninit<SlotEntry>]) -> usize {
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

    /// Release a batch of slots under a single lock acquisition.
    pub fn release_batch(&mut self, entries: &[SlotEntry]) {
        for entry in entries {
            self.release_one(*entry);
        }
    }

    /// Permanently mark a slot as allocated (removes it from the free
    /// pool forever).  Used at init to reserve one sentinel slot per
    /// 2 MB chunk — the sentinel's mapping forces the intermediate
    /// page table into existence so runtime allocations never race on
    /// PT creation.
    pub fn reserve_sentinel(&mut self, idx: u32) {
        let slot = idx as usize;
        debug_assert!(slot < R::MAX_SLOTS);
        let word_idx = slot / 64;
        let bit = slot % 64;
        let mask = 1u64 << bit;
        self.free_bitmap[word_idx] &= !mask;
        self.backed_bitmap[word_idx] |= mask;
    }

    /// Number of slots currently outside the global free pool — sum over
    /// dropped-into-PCP and live `StackSlot<R>` handles.
    #[inline]
    pub fn in_use(&self) -> u32 {
        self.in_use
    }
}

// ---------------------------------------------------------------------------
// Per-CPU cache (one array per region).
// ---------------------------------------------------------------------------

/// One CPU's stack-slot cache.  `stack_idx[0..count]` and
/// `stack_backed[0..count]` are parallel arrays.  Access requires a
/// `PreemptGuard` for CPU pinning; the atomic stat counters are safe
/// to read from any CPU.
#[repr(C, align(64))]
#[allow(dead_code)]
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

#[allow(dead_code)]
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

/// Sync wrapper around a `[PerCpuStackCache<R, CAP>; MAX_CPUS]`.
///
/// SAFETY: each CPU only mutates its own slot while pinned by
/// `PreemptGuard`; cross-CPU reads are restricted to the atomic stat
/// fields.
#[allow(dead_code)]
pub struct PcpArray<R: StackRegion, const CAP: usize>(
    UnsafeCell<[PerCpuStackCache<R, CAP>; MAX_CPUS]>,
);

unsafe impl<R: StackRegion, const CAP: usize> Sync for PcpArray<R, CAP> {}

#[allow(dead_code)]
impl<R: StackRegion, const CAP: usize> PcpArray<R, CAP> {
    pub const fn new(init: [PerCpuStackCache<R, CAP>; MAX_CPUS]) -> Self {
        Self(UnsafeCell::new(init))
    }

    /// # Safety
    /// Caller must hold a `PreemptGuard` so `cpu` (== current CPU) stays
    /// stable for the lifetime of the returned reference.
    #[inline]
    pub unsafe fn cache(&self, cpu: usize) -> &mut PerCpuStackCache<R, CAP> {
        debug_assert!(
            PreemptGuard::is_active(),
            "stack_va PcpArray::cache requires PreemptGuard"
        );
        debug_assert!(cpu < MAX_CPUS);
        unsafe { &mut (*self.0.get())[cpu] }
    }

    /// Read another CPU's cache for diagnostics.  Benign data race on
    /// non-atomic fields; the atomics give a consistent snapshot of the
    /// counters.
    pub fn snapshot(&self, cpu: usize) -> PcpStats {
        if cpu >= MAX_CPUS {
            return PcpStats::default();
        }
        // SAFETY: read-only access for diagnostics.  count is a benign race.
        let cache = unsafe { &(*self.0.get())[cpu] };
        PcpStats {
            count: cache.count,
            alloc_count: cache.alloc_count.load(Ordering::Relaxed),
            free_count: cache.free_count.load(Ordering::Relaxed),
            refill_count: cache.refill_count.load(Ordering::Relaxed),
            spill_count: cache.spill_count.load(Ordering::Relaxed),
        }
    }

    /// # Safety
    /// Shutdown only — no concurrent PCP activity assumed.
    pub unsafe fn cache_unchecked(&self, cpu: usize) -> &mut PerCpuStackCache<R, CAP> {
        debug_assert!(cpu < MAX_CPUS);
        unsafe { &mut (*self.0.get())[cpu] }
    }
}

// ---------------------------------------------------------------------------
// Per-region wiring helpers (called from each region's `_*` trait impls).
// ---------------------------------------------------------------------------

/// Refill the current CPU's cache from the global allocator.  Runs with
/// one lock acquisition for the batch.
///
/// # Safety
/// Caller must hold a `PreemptGuard`.
#[allow(dead_code)]
pub fn pcp_refill<R: StackRegion, const WORDS: usize, const CAP: usize, const REFILL: usize>(
    global: &IrqMutex<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
    cpu: usize,
) {
    debug_assert!(
        PreemptGuard::is_active(),
        "stack_va::pcp_refill requires PreemptGuard"
    );
    debug_assert!(REFILL <= CAP);
    // SAFETY: PreemptGuard pins us to `cpu`.
    let cache = unsafe { pcp.cache(cpu) };
    if cache.count as usize >= REFILL {
        return;
    }

    let room = CAP - cache.count as usize;
    let want = REFILL.min(room);
    if want == 0 {
        return;
    }

    let mut batch: [MaybeUninit<SlotEntry>; REFILL] = [const { MaybeUninit::uninit() }; REFILL];
    let got = {
        let mut alloc = global.lock();
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
#[allow(dead_code)]
pub fn pcp_spill<R: StackRegion, const WORDS: usize, const CAP: usize, const SPILL: usize>(
    global: &IrqMutex<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
    cpu: usize,
) {
    debug_assert!(
        PreemptGuard::is_active(),
        "stack_va::pcp_spill requires PreemptGuard"
    );
    debug_assert!(SPILL < CAP);
    // SAFETY: PreemptGuard pins us.
    let cache = unsafe { pcp.cache(cpu) };
    let want = SPILL.min(cache.count as usize);
    if want == 0 {
        return;
    }

    // Spill from the bottom (oldest) entries so the hot tail stays
    // cache-resident.  Costs one memmove per spill but keeps LIFO
    // locality on the surviving entries.
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

/// Drain every CPU's cache back to the global allocator.  Shutdown
/// only — no concurrent PCP activity assumed.
#[allow(dead_code)]
pub fn pcp_drain_all_impl<
    R: StackRegion,
    const WORDS: usize,
    const CAP: usize,
    const SPILL: usize,
>(
    global: &IrqMutex<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
) {
    for cpu in 0..MAX_CPUS {
        // SAFETY: shutdown-only — no concurrent access.
        let cache = unsafe { pcp.cache_unchecked(cpu) };
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
}

/// Drain the current CPU's cache back to the global allocator.  Test
/// helper used to make global-bitmap state observable without chasing
/// PCP effects.
#[allow(dead_code)]
pub fn pcp_flush_current_impl<
    R: StackRegion,
    const WORDS: usize,
    const CAP: usize,
    const SPILL: usize,
>(
    global: &IrqMutex<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
) {
    let _no_migrate = PreemptGuard::new();
    let cpu = slopos_arch::pcr::get_current_cpu();
    // SAFETY: PreemptGuard pins us.
    let cache = unsafe { pcp.cache(cpu) };
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

/// Allocator-side of slot pop: pop from this CPU's cache (refilling
/// from the global if empty).  Called by the per-region `_slot_pop`
/// wiring.  Runs under a single `PreemptGuard`.
#[allow(dead_code)]
pub fn slot_pop_impl<R: StackRegion, const WORDS: usize, const CAP: usize, const REFILL: usize>(
    global: &IrqMutex<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
) -> Option<SlotEntry> {
    let _no_migrate = PreemptGuard::new();
    let cpu = slopos_arch::pcr::get_current_cpu();
    // SAFETY: PreemptGuard pins us.
    let cache = unsafe { pcp.cache(cpu) };

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

/// Allocator-side of slot push: push back to this CPU's cache
/// (spilling to the global if full).  Called by the per-region
/// `_slot_push` wiring.  Runs under a single `PreemptGuard`.
#[allow(dead_code)]
pub fn slot_push_impl<R: StackRegion, const WORDS: usize, const CAP: usize, const SPILL: usize>(
    global: &IrqMutex<StackVaAllocator<R, WORDS>>,
    pcp: &PcpArray<R, CAP>,
    entry: SlotEntry,
) {
    let _no_migrate = PreemptGuard::new();
    let cpu = slopos_arch::pcr::get_current_cpu();
    // SAFETY: PreemptGuard pins us.
    let cache = unsafe { pcp.cache(cpu) };

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

/// Initialise the global allocator and install one page-table sentinel
/// per 2 MB chunk in the region.  The sentinel mapping forces the
/// intermediate page table into existence so runtime allocations never
/// race on PT creation.
///
/// Panics if the page allocator runs out of frames or a sentinel
/// mapping fails — both indicate severe boot-time misconfiguration.
#[allow(dead_code)]
pub fn init_with_sentinels<R: StackRegion, const WORDS: usize>(
    global: &IrqMutex<StackVaAllocator<R, WORDS>>,
) {
    {
        let mut alloc = global.lock();
        alloc.init();
    }
    install_pt_sentinels::<R, WORDS>(global);
}

#[allow(dead_code)]
fn install_pt_sentinels<R: StackRegion, const WORDS: usize>(
    global: &IrqMutex<StackVaAllocator<R, WORDS>>,
) {
    let slots_per_chunk = (PAGE_SIZE_2MB / R::STRIDE) as u32;
    let chunk_count = ((R::VA_END - R::VA_BASE) / PAGE_SIZE_2MB) as u32;
    let flags = R::PAGE_FLAGS;

    for chunk in 0..chunk_count {
        let sentinel_slot = chunk * slots_per_chunk;
        global.lock().reserve_sentinel(sentinel_slot);

        let pa = alloc_page_frame(0);
        if pa.is_null() {
            panic!(
                "stack_va::{}::init: out of frames for sentinel (chunk {})",
                R::NAME,
                chunk
            );
        }
        let va = VirtAddr::new(R::VA_BASE + sentinel_slot as u64 * R::STRIDE);
        if map_page_4kb(va, pa, flags) != 0 {
            panic!(
                "stack_va::{}::init: failed to map sentinel at {:#x}",
                R::NAME,
                va.as_u64()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Public top-level API.  Routes through the per-region trait wiring.
// ---------------------------------------------------------------------------

/// Allocate a free slot from region `R`.  Returns `None` only when the
/// region is genuinely exhausted.
#[inline]
#[allow(dead_code)]
pub fn alloc_slot<R: StackRegion>() -> Option<StackSlot<R>> {
    R::_slot_pop().map(StackSlot::from_entry)
}

/// One-shot boot-time initialisation for region `R`.  Idempotent.
#[inline]
#[allow(dead_code)]
pub fn init<R: StackRegion>() {
    R::_init();
}

/// Total slots outside the global free pool — the sum of slots held in
/// any CPU's cache plus slots held by live `StackSlot<R>` handles.
#[inline]
#[allow(dead_code)]
pub fn in_use_count<R: StackRegion>() -> u32 {
    R::_in_use_count()
}

/// Drain every CPU's cache back to the global pool.  Shutdown only.
#[inline]
#[allow(dead_code)]
pub fn pcp_drain_all<R: StackRegion>() {
    R::_pcp_drain_all();
}

/// Drain the current CPU's cache.  Test helper.
#[inline]
#[allow(dead_code)]
pub fn pcp_flush_current<R: StackRegion>() {
    R::_pcp_flush_current();
}

/// Diagnostic snapshot of one CPU's cache state.
#[inline]
#[allow(dead_code)]
pub fn pcp_stats<R: StackRegion>(cpu: usize) -> PcpStats {
    R::_pcp_stats(cpu)
}

/// Per-CPU cache capacity for region `R`.
#[inline]
#[allow(dead_code)]
pub const fn pcp_capacity<R: StackRegion>() -> usize {
    R::PCP_CAPACITY
}
