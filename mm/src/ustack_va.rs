//! Unsafe-stack virtual-address allocator.
//!
//! Mirrors [`crate::kstack_va`] one-for-one but manages the dedicated
//! SafeStack data-stack VA region (`USTACK_VA_BASE..USTACK_VA_END`).  The
//! two regions deliberately sit in disjoint VA ranges so an address-taken
//! pointer that leaks from the unsafe stack cannot alias the safe
//! (kernel) stack — which is what makes the SafeStack partitioning an
//! effective ROP defence.
//!
//! Structure (matches `kstack_va` exactly):
//!
//! - Global bitmap-backed allocator under `IrqMutex` is the source of truth.
//! - Per-CPU cache in front of the global drains one batch under a lock per
//!   refill / spill; the warm path runs under `PreemptGuard` only.
//! - `UstackSlot` RAII handle returns the slot to the per-CPU cache on drop.
//!
//! Separating this from `kstack_va` — rather than sharing one allocator
//! with two stride zones — keeps the lock-free warm path lock-free even
//! when both safe and unsafe stacks churn on the same CPU.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::VirtAddr;
use slopos_arch::pcr::MAX_CPUS;
use slopos_sync::{IrqMutex, LOCK_LEVEL_ALLOCATOR, PreemptGuard};

use crate::memory_layout_defs::{USTACK_MAX_SLOTS, USTACK_STRIDE, USTACK_VA_BASE, USTACK_VA_END};
use crate::page_alloc::alloc_page_frame;
use crate::paging::map_page_4kb;
use crate::paging_defs::{PAGE_SIZE_2MB, PageFlags};

const BITMAP_WORDS: usize = (USTACK_MAX_SLOTS + 63) / 64;

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

#[derive(Clone, Copy)]
struct SlotEntry {
    idx: u32,
    backed: bool,
}

struct UstackVaAllocator {
    free_bitmap: [u64; BITMAP_WORDS],
    backed_bitmap: [u64; BITMAP_WORDS],
    hint: u32,
    in_use: u32,
    ready: bool,
}

impl UstackVaAllocator {
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
        if total_bits > USTACK_MAX_SLOTS {
            let extra = total_bits - USTACK_MAX_SLOTS;
            let last = BITMAP_WORDS - 1;
            let keep = 64 - extra;
            let mask = if keep == 0 { 0 } else { !0u64 >> (64 - keep) };
            self.free_bitmap[last] &= mask;
        }
        self.hint = 0;
        self.in_use = 0;
        self.ready = true;
    }

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
            if slot >= USTACK_MAX_SLOTS {
                continue;
            }
            let mask = 1u64 << bit;
            self.free_bitmap[word_idx] &= !mask;
            let backed = self.backed_bitmap[word_idx] & mask != 0;
            self.in_use += 1;
            self.hint = ((slot + 1) % USTACK_MAX_SLOTS) as u32;
            return Some(SlotEntry {
                idx: slot as u32,
                backed,
            });
        }
        None
    }

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

    fn release_one(&mut self, entry: SlotEntry) {
        let slot = entry.idx as usize;
        debug_assert!(slot < USTACK_MAX_SLOTS, "ustack_va: release out of range");
        let word_idx = slot / 64;
        let bit = slot % 64;
        let mask = 1u64 << bit;
        debug_assert!(
            self.free_bitmap[word_idx] & mask == 0,
            "ustack_va: double-release of slot {}",
            slot
        );
        if entry.backed {
            self.backed_bitmap[word_idx] |= mask;
        }
        self.free_bitmap[word_idx] |= mask;
        self.in_use = self.in_use.saturating_sub(1);
    }

    fn release_batch(&mut self, entries: &[SlotEntry]) {
        for entry in entries {
            self.release_one(*entry);
        }
    }

    fn reserve_sentinel(&mut self, idx: u32) {
        let slot = idx as usize;
        debug_assert!(slot < USTACK_MAX_SLOTS);
        let word_idx = slot / 64;
        let bit = slot % 64;
        let mask = 1u64 << bit;
        self.free_bitmap[word_idx] &= !mask;
        self.backed_bitmap[word_idx] |= mask;
    }
}

static USTACK_VA_ALLOCATOR: IrqMutex<UstackVaAllocator> =
    IrqMutex::new(UstackVaAllocator::new_uninit(), LOCK_LEVEL_ALLOCATOR);

// ---------------------------------------------------------------------------
// Per-CPU ustack-slot cache.
// ---------------------------------------------------------------------------

const USTACK_PCP_CAPACITY: usize = 16;
const USTACK_PCP_REFILL_BATCH: usize = 8;
const USTACK_PCP_SPILL_BATCH: usize = 8;

#[repr(C, align(64))]
struct PerCpuUstackCache {
    stack_idx: [u32; USTACK_PCP_CAPACITY],
    stack_backed: [bool; USTACK_PCP_CAPACITY],
    count: u32,
    alloc_count: AtomicU32,
    free_count: AtomicU32,
    refill_count: AtomicU32,
    spill_count: AtomicU32,
}

impl PerCpuUstackCache {
    const fn new() -> Self {
        Self {
            stack_idx: [u32::MAX; USTACK_PCP_CAPACITY],
            stack_backed: [false; USTACK_PCP_CAPACITY],
            count: 0,
            alloc_count: AtomicU32::new(0),
            free_count: AtomicU32::new(0),
            refill_count: AtomicU32::new(0),
            spill_count: AtomicU32::new(0),
        }
    }
}

struct PcpArray(UnsafeCell<[PerCpuUstackCache; MAX_CPUS]>);
unsafe impl Sync for PcpArray {}

static PER_CPU_CACHES: PcpArray = {
    const INIT: PerCpuUstackCache = PerCpuUstackCache::new();
    PcpArray(UnsafeCell::new([INIT; MAX_CPUS]))
};

#[inline]
fn get_current_cpu() -> usize {
    slopos_arch::pcr::get_current_cpu()
}

#[inline]
unsafe fn pcp_cache(cpu: usize) -> &'static mut PerCpuUstackCache {
    debug_assert!(
        PreemptGuard::is_active(),
        "ustack pcp_cache requires PreemptGuard"
    );
    debug_assert!(cpu < MAX_CPUS);
    unsafe { &mut (*PER_CPU_CACHES.0.get())[cpu] }
}

fn ustack_pcp_refill(cpu: usize) {
    debug_assert!(
        PreemptGuard::is_active(),
        "ustack_pcp_refill requires PreemptGuard"
    );

    let cache = unsafe { pcp_cache(cpu) };
    if cache.count as usize >= USTACK_PCP_REFILL_BATCH {
        return;
    }

    let room = USTACK_PCP_CAPACITY - cache.count as usize;
    let want = USTACK_PCP_REFILL_BATCH.min(room);
    if want == 0 {
        return;
    }

    let mut batch: [MaybeUninit<SlotEntry>; USTACK_PCP_REFILL_BATCH] =
        [const { MaybeUninit::uninit() }; USTACK_PCP_REFILL_BATCH];
    let got = {
        let mut alloc = USTACK_VA_ALLOCATOR.lock();
        alloc.alloc_batch(&mut batch[..want])
    };

    for entry_slot in batch.iter().take(got) {
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

fn ustack_pcp_spill(cpu: usize) {
    debug_assert!(
        PreemptGuard::is_active(),
        "ustack_pcp_spill requires PreemptGuard"
    );

    let cache = unsafe { pcp_cache(cpu) };
    let want = USTACK_PCP_SPILL_BATCH.min(cache.count as usize);
    if want == 0 {
        return;
    }

    let mut batch: [SlotEntry; USTACK_PCP_SPILL_BATCH] = [SlotEntry {
        idx: u32::MAX,
        backed: false,
    }; USTACK_PCP_SPILL_BATCH];
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
        let mut alloc = USTACK_VA_ALLOCATOR.lock();
        alloc.release_batch(&batch[..want]);
    }
    cache.spill_count.fetch_add(1, Ordering::Relaxed);
}

pub fn ustack_pcp_drain_all() {
    for cpu in 0..MAX_CPUS {
        let cache = unsafe { &mut (*PER_CPU_CACHES.0.get())[cpu] };
        while cache.count > 0 {
            let n = cache.count as usize;
            let take = USTACK_PCP_SPILL_BATCH.min(n);
            let mut batch: [SlotEntry; USTACK_PCP_SPILL_BATCH] = [SlotEntry {
                idx: u32::MAX,
                backed: false,
            };
                USTACK_PCP_SPILL_BATCH];
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
            let mut alloc = USTACK_VA_ALLOCATOR.lock();
            alloc.release_batch(&batch[..take]);
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct UstackPcpStats {
    pub count: u32,
    pub alloc_count: u32,
    pub free_count: u32,
    pub refill_count: u32,
    pub spill_count: u32,
}

pub fn ustack_pcp_stats(cpu: usize) -> UstackPcpStats {
    if cpu >= MAX_CPUS {
        return UstackPcpStats::default();
    }
    let cache = unsafe { &(*PER_CPU_CACHES.0.get())[cpu] };
    UstackPcpStats {
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

pub struct UstackSlot {
    idx: u32,
    was_backed: bool,
}

impl UstackSlot {
    #[inline]
    pub fn va_base(&self) -> VirtAddr {
        VirtAddr::new(USTACK_VA_BASE + self.idx as u64 * USTACK_STRIDE)
    }

    #[inline]
    pub fn va_end(&self) -> VirtAddr {
        VirtAddr::new(self.va_base().as_u64() + USTACK_STRIDE)
    }

    #[inline]
    pub fn index(&self) -> u32 {
        self.idx
    }

    #[inline]
    pub fn was_backed(&self) -> bool {
        self.was_backed
    }

    pub fn mark_backed(&mut self) {
        self.was_backed = true;
    }
}

impl Drop for UstackSlot {
    fn drop(&mut self) {
        let _no_migrate = PreemptGuard::new();
        let cpu = get_current_cpu();
        let cache = unsafe { pcp_cache(cpu) };

        if cache.count as usize >= USTACK_PCP_CAPACITY {
            ustack_pcp_spill(cpu);
        }

        debug_assert!((cache.count as usize) < USTACK_PCP_CAPACITY);
        let c = cache.count as usize;
        cache.stack_idx[c] = self.idx;
        cache.stack_backed[c] = self.was_backed;
        cache.count += 1;
        cache.free_count.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn alloc_slot() -> Option<UstackSlot> {
    let _no_migrate = PreemptGuard::new();
    let cpu = get_current_cpu();
    let cache = unsafe { pcp_cache(cpu) };

    if cache.count == 0 {
        ustack_pcp_refill(cpu);
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

    Some(UstackSlot { idx, was_backed })
}

// ---------------------------------------------------------------------------
// Init / sentinels.
// ---------------------------------------------------------------------------

pub fn init() {
    {
        let mut alloc = USTACK_VA_ALLOCATOR.lock();
        alloc.init();
    }
    install_pt_sentinels();
}

fn install_pt_sentinels() {
    let slots_per_chunk = (PAGE_SIZE_2MB / USTACK_STRIDE) as u32;
    let chunk_count = ((USTACK_VA_END - USTACK_VA_BASE) / PAGE_SIZE_2MB) as u32;

    let flags = (PageFlags::KERNEL_RW | PageFlags::NO_EXECUTE).bits();

    for chunk in 0..chunk_count {
        let sentinel_slot = chunk * slots_per_chunk;
        USTACK_VA_ALLOCATOR.lock().reserve_sentinel(sentinel_slot);

        let pa = alloc_page_frame(0);
        if pa.is_null() {
            panic!(
                "ustack_va::init: out of frames for sentinel (chunk {})",
                chunk
            );
        }
        let va = VirtAddr::new(USTACK_VA_BASE + sentinel_slot as u64 * USTACK_STRIDE);
        if map_page_4kb(va, pa, flags) != 0 {
            panic!(
                "ustack_va::init: failed to map sentinel at {:#x}",
                va.as_u64()
            );
        }
    }
}

pub fn in_use_count() -> u32 {
    USTACK_VA_ALLOCATOR.lock().in_use
}

pub fn ustack_pcp_flush_current() {
    let _no_migrate = PreemptGuard::new();
    let cpu = get_current_cpu();
    let cache = unsafe { pcp_cache(cpu) };
    while cache.count > 0 {
        let n = cache.count as usize;
        let take = USTACK_PCP_SPILL_BATCH.min(n);
        let mut batch: [SlotEntry; USTACK_PCP_SPILL_BATCH] = [SlotEntry {
            idx: u32::MAX,
            backed: false,
        }; USTACK_PCP_SPILL_BATCH];
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
        let mut alloc = USTACK_VA_ALLOCATOR.lock();
        alloc.release_batch(&batch[..take]);
    }
}

pub const fn pcp_capacity() -> usize {
    USTACK_PCP_CAPACITY
}
