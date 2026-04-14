//! Kernel stack virtual-address allocator.
//!
//! Manages the `KSTACK_VA_BASE..KSTACK_VA_END` region as a bitmap of
//! fixed-stride slots, each large enough to hold one task's kernel stack
//! (including a guard page).  This layer deals **only** with virtual
//! address ranges — physical frames and PTE mapping live in the
//! `KernelStack` type one layer up (`core::scheduler::stack`).
//!
//! The design mirrors the existing IST-stack pattern in
//! `boot/src/ist_stacks.rs`, but generalised for dynamic allocation.
//!
//! # Memory decoupling
//!
//! The backing region is a fixed slice of kernel virtual address space
//! declared in `memory_layout_defs`.  Growing the kernel image moves
//! `_kernel_end` but has no effect on this region — task-stack capacity
//! is therefore independent of kernel binary size.  This is the fix for
//! the SUITE61 regression described in
//! `plans/KERNEL_STACK_ALLOCATION_OVERHAUL.md`.
//!
//! # Safety
//!
//! The VA allocator is 100% safe Rust.  It does not touch page tables
//! or physical memory; it only hands out opaque [`KstackSlot`] handles
//! that name a virtual range within the region.

use slopos_abi::addr::VirtAddr;
use slopos_sync::{IrqMutex, LOCK_LEVEL_ALLOCATOR};

use crate::memory_layout_defs::{KSTACK_MAX_SLOTS, KSTACK_STRIDE, KSTACK_VA_BASE, KSTACK_VA_END};
use crate::page_alloc::alloc_page_frame;
use crate::paging::map_page_4kb;
use crate::paging_defs::{PAGE_SIZE_2MB, PageFlags};

/// Words of `u64` needed to cover `KSTACK_MAX_SLOTS` bits.
///
/// `KSTACK_MAX_SLOTS = 8192` → 128 u64 words.  `+ 63` handles any slot
/// count that isn't a multiple of 64.
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

/// Allocator state.
///
/// Two bitmaps track the lifecycle of each slot:
///
/// - `free_bitmap`:  `1` = slot is free for reuse, `0` = currently in use.
/// - `backed_bitmap`: `1` = slot has physical frames mapped (cached from a
///   previous allocation), `0` = virgin (no backing yet).
///
/// # Why keep backing after free
///
/// Unmapping a kernel-space page triggers a broadcast TLB shootdown IPI
/// to every CPU (see `mm::tlb::flush_page`).  Under task churn, that
/// floods the IPI path and can stall shootdown handlers.  Linux solves
/// this with per-CPU stack caches (`CONFIG_VMAP_STACK` + task cache).
///
/// We use a simpler version: once a slot is mapped, keep the mappings
/// alive forever.  `KernelStack::drop` just flips the free bit; no
/// `unmap_page`, no frame free, no TLB shootdown.  The next `alloc` for
/// the same slot reuses the same frames (after zero-filling for hygiene).
///
/// Memory footprint: peak concurrent tasks × 32 KB.  For a typical
/// workload this stays below 10 MB.  Phase 2 will add an eviction
/// policy if this becomes an issue.
struct KstackVaAllocator {
    free_bitmap: [u64; BITMAP_WORDS],
    backed_bitmap: [u64; BITMAP_WORDS],
    /// Rotating search hint to avoid scanning from the start every alloc.
    hint: u32,
    /// Count of currently-allocated slots (for stats/debug).
    in_use: u32,
    /// Peak count of backed slots (stats only).
    peak_backed: u32,
    /// Set once `init()` has initialised the bitmaps.
    ready: bool,
}

impl KstackVaAllocator {
    const fn new_uninit() -> Self {
        Self {
            free_bitmap: [0u64; BITMAP_WORDS],
            backed_bitmap: [0u64; BITMAP_WORDS],
            hint: 0,
            in_use: 0,
            peak_backed: 0,
            ready: false,
        }
    }

    /// Mark every slot as free (none allocated, none backed).  Called
    /// once from [`init`].
    fn init(&mut self) {
        for w in self.free_bitmap.iter_mut() {
            *w = !0u64;
        }
        for w in self.backed_bitmap.iter_mut() {
            *w = 0;
        }
        // Clear any tail bits beyond KSTACK_MAX_SLOTS (if the count isn't a
        // multiple of 64) so `alloc` can't hand them out.
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
        self.peak_backed = 0;
        self.ready = true;
    }

    /// Find and claim a free slot.
    ///
    /// Returns `(idx, was_backed)`.  When `was_backed` is `true`, the slot
    /// already has mapped frames from a previous allocation and the caller
    /// only needs to re-zero the stack contents.  When `false`, the caller
    /// must allocate physical frames, map them, and invoke
    /// [`mark_backed`] once the mapping is complete.
    fn alloc(&mut self) -> Option<(u32, bool)> {
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
            let was_backed = self.backed_bitmap[word_idx] & mask != 0;
            self.in_use += 1;
            self.hint = ((slot + 1) % KSTACK_MAX_SLOTS) as u32;
            return Some((slot as u32, was_backed));
        }
        None
    }

    /// Record that slot `idx` now has physical backing (pages mapped).
    fn mark_backed(&mut self, idx: u32) {
        let slot = idx as usize;
        debug_assert!(slot < KSTACK_MAX_SLOTS);
        let word_idx = slot / 64;
        let bit = slot % 64;
        let was = self.backed_bitmap[word_idx] & (1u64 << bit) != 0;
        self.backed_bitmap[word_idx] |= 1u64 << bit;
        if !was {
            // Count new backing and track peak.
            let backed = self.count_backed();
            if backed > self.peak_backed {
                self.peak_backed = backed;
            }
        }
    }

    fn count_backed(&self) -> u32 {
        let mut n = 0u32;
        for w in self.backed_bitmap.iter() {
            n += w.count_ones();
        }
        n
    }

    /// Return a slot to the free pool.  Keeps any existing backing in
    /// place so the next allocation for this slot can reuse it.
    fn release(&mut self, idx: u32) {
        let slot = idx as usize;
        debug_assert!(slot < KSTACK_MAX_SLOTS, "kstack_va: release out of range");
        let word_idx = slot / 64;
        let bit = slot % 64;
        let mask = 1u64 << bit;
        debug_assert!(
            self.free_bitmap[word_idx] & mask == 0,
            "kstack_va: double-release of slot {}",
            slot
        );
        self.free_bitmap[word_idx] |= mask;
        self.in_use = self.in_use.saturating_sub(1);
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
        // Mark as backed so nothing tries to re-map it.
        self.backed_bitmap[word_idx] |= mask;
    }
}

/// Global state.
static KSTACK_VA_ALLOCATOR: IrqMutex<KstackVaAllocator> =
    IrqMutex::new(KstackVaAllocator::new_uninit(), LOCK_LEVEL_ALLOCATOR);

/// RAII handle owning one slot in the kernel-stack VA region.
///
/// The slot is returned to the allocator automatically on drop.  The
/// handle itself does not map any physical memory — that is handled by
/// `core::scheduler::stack::KernelStack`, which owns a `KstackSlot` and
/// drives the page-table operations.
///
/// On first allocation of a given slot, `was_backed == false` signals
/// that the caller needs to allocate frames and map them.  On reuse,
/// `was_backed == true` means the frames are still mapped from a
/// previous lifecycle — just zero and go.
///
/// `KstackSlot` is deliberately **not** `Clone`/`Copy` so double-release
/// is impossible by construction.
pub struct KstackSlot {
    idx: u32,
    /// `true` if the slot was already mapped when allocated (cached).
    was_backed: bool,
}

impl KstackSlot {
    /// Lowest virtual address of this slot's stride (guard-page base).
    #[inline]
    pub fn va_base(&self) -> VirtAddr {
        VirtAddr::new(KSTACK_VA_BASE + self.idx as u64 * KSTACK_STRIDE)
    }

    /// One-past-last virtual address of this slot's stride.
    #[inline]
    pub fn va_end(&self) -> VirtAddr {
        VirtAddr::new(self.va_base().as_u64() + KSTACK_STRIDE)
    }

    /// Slot index (exposed for diagnostic logging only).
    #[inline]
    pub fn index(&self) -> u32 {
        self.idx
    }

    /// Was this slot already mapped at alloc time?
    #[inline]
    pub fn was_backed(&self) -> bool {
        self.was_backed
    }

    /// After the caller successfully maps physical frames into this slot's
    /// VA range, call this to record that the slot is now backed.  Safe to
    /// call even if already backed (idempotent).
    pub fn mark_backed(&mut self) {
        KSTACK_VA_ALLOCATOR.lock().mark_backed(self.idx);
        self.was_backed = true;
    }
}

impl Drop for KstackSlot {
    fn drop(&mut self) {
        KSTACK_VA_ALLOCATOR.lock().release(self.idx);
    }
}

/// Allocate a free slot from the kernel-stack VA region.
///
/// Returns `None` if the region is full or `init()` has not run yet.
pub fn alloc_slot() -> Option<KstackSlot> {
    KSTACK_VA_ALLOCATOR
        .lock()
        .alloc()
        .map(|(idx, was_backed)| KstackSlot { idx, was_backed })
}

/// Initialise the allocator.  Call once from memory-system bring-up,
/// after paging is online.
///
/// Two jobs:
///
/// 1. Initialise the bitmap to all-free, all-unbacked.
/// 2. Pre-reserve **one sentinel slot per 2 MB PD chunk** across the
///    KSTACK region, then map a sentinel page into each — forcing the
///    intermediate page table for that chunk to be allocated *now*,
///    while boot is still single-threaded.
///
/// Why: `mm::paging::map_page_in_directory` is not SMP-safe for
/// intermediate-table creation — two CPUs can both see
/// `pd_entry.is_present() == false`, both allocate a PT frame, and
/// the loser's PT gets orphaned (its just-installed leaf entries
/// vanish).  That's exactly the race we hit when two APs created
/// their idle tasks concurrently: each mapped a KSTACK page, one
/// CPU's PT won, the other CPU's page fault'd on the missing PTE.
///
/// Fix: pre-create every PT the KSTACK region will ever need, at
/// boot, while only the BSP is running.  After init, every
/// `map_page_4kb` call into KSTACK only sets a leaf PTE — an atomic
/// 8-byte store, no race possible.
///
/// Cost: 256 sentinel pages + 256 PT pages = ~2 MB of physical
/// memory permanently reserved, and 256 slots (one per chunk) lost
/// to sentinels.  With 8192 slots total and MAX_TASKS=256, plenty
/// of headroom remains.
pub fn init() {
    {
        let mut alloc = KSTACK_VA_ALLOCATOR.lock();
        alloc.init();
    }
    install_pt_sentinels();
}

fn install_pt_sentinels() {
    // One slot per 2 MB PD chunk.
    let slots_per_chunk = (PAGE_SIZE_2MB / KSTACK_STRIDE) as u32;
    let chunk_count = ((KSTACK_VA_END - KSTACK_VA_BASE) / PAGE_SIZE_2MB) as u32;

    let flags = (PageFlags::KERNEL_RW | PageFlags::NO_EXECUTE).bits();

    for chunk in 0..chunk_count {
        let sentinel_slot = chunk * slots_per_chunk;
        // Reserve the slot so `alloc_slot` never hands it out.
        KSTACK_VA_ALLOCATOR.lock().reserve_sentinel(sentinel_slot);

        // Map one sentinel page inside the reserved slot's VA range.
        // This forces the PD entry to point at a freshly-allocated PT.
        // The sentinel is never unmapped, so the PT stays installed
        // for the kernel's lifetime.
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

/// Number of slots currently allocated.  Diagnostic / test-only helper.
pub fn in_use_count() -> u32 {
    KSTACK_VA_ALLOCATOR.lock().in_use
}
