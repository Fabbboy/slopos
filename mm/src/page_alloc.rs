//! Physical Page Frame Allocator with Per-CPU Page Caches (PCP)
//!
//! This module provides a buddy allocator for physical page frames with
//! per-CPU page caches for order-0 (single page) allocations. The PCP
//! layer reduces lock contention by caching recently freed pages locally
//! per CPU, avoiding the global lock for common allocation patterns.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    alloc_page_frame()                           │
//! │                           │                                     │
//! │              ┌────────────┴────────────┐                        │
//! │              │    Order == 0?          │                        │
//! │              └────────────┬────────────┘                        │
//! │                   Yes     │      No                             │
//! │              ┌────────────┴────────────┐                        │
//! │              ▼                         ▼                        │
//! │   ┌─────────────────────┐   ┌─────────────────────┐            │
//! │   │ Per-CPU Page Cache  │   │   Buddy Allocator   │            │
//! │   │   (lock-free)       │   │   (global lock)     │            │
//! │   └─────────┬───────────┘   └─────────────────────┘            │
//! │             │ Empty?                                            │
//! │             ▼                                                   │
//! │   ┌─────────────────────┐                                      │
//! │   │ Refill from Buddy   │                                      │
//! │   │ (batch allocation)  │                                      │
//! │   └─────────────────────┘                                      │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Per-CPU Cache Benefits
//!
//! - **Reduced lock contention**: Order-0 alloc/free often avoids global lock
//! - **Cache locality**: Recently freed pages stay hot in CPU cache
//! - **Batch operations**: Refill/drain multiple pages at once to amortize lock cost

use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_arch::pcr::MAX_CPUS;
use slopos_ostd::sync::cpu_local::{CacheAligned, CpuLocal};
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_ALLOCATOR, PreemptGuard, RawTable, SpinLock};
use slopos_utils::{align_down_u64, align_up_u64, klog_debug, klog_info};

use crate::hhdm::PhysAddrHhdm;
use crate::memory_reservations::{
    MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS, MmRegion, MmRegionKind, mm_region_count, mm_region_get,
    mm_reservations_count, mm_reservations_get,
};
use crate::paging_defs::PAGE_SIZE_4KB;

pub const ALLOC_FLAG_DMA: u32 = 0x02;
pub const ALLOC_FLAG_KERNEL: u32 = 0x04;
pub const ALLOC_FLAG_ORDER_SHIFT: u32 = 8;
pub const ALLOC_FLAG_ORDER_MASK: u32 = 0x1F << ALLOC_FLAG_ORDER_SHIFT;
pub const ALLOC_FLAG_NO_PCP: u32 = 0x80;
const PAGE_FRAME_FREE: u8 = 0x00;
const PAGE_FRAME_ALLOCATED: u8 = 0x01;
const PAGE_FRAME_RESERVED: u8 = 0x02;
const PAGE_FRAME_KERNEL: u8 = 0x03;
const PAGE_FRAME_DMA: u8 = 0x04;
const PAGE_FRAME_PCP: u8 = 0x05;

const INVALID_PAGE_FRAME: u32 = 0xFFFF_FFFF;
const MAX_ORDER: u32 = 24;
const INVALID_REGION_ID: u16 = 0xFFFF;

const PCP_CAPACITY: usize = 64;
const PCP_LOW_WATERMARK: u32 = 8;
const PCP_HIGH_WATERMARK: u32 = PCP_CAPACITY as u32;
const PCP_BATCH_SIZE: u32 = 16;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PageFrame {
    ref_count: u32,
    state: u8,
    flags: u8,
    order: u16,
    region_id: u16,
    next_free: u32,
}

// ---------------------------------------------------------------------------
// Per-CPU page cache — stack-based, lock-free on the fast path.
//
// Frame numbers are stored directly in a per-CPU array. The alloc fast path
// pops from the stack with only a PreemptGuard (no global lock). The free
// path still takes the global lock for ref_count safety but pushes to the
// stack instead of threading through the global frame descriptor linked list.
//
// The global lock is only taken for:
//   - Batch refill (when the stack is empty)
//   - Batch drain  (when the stack overflows or on shutdown)
//   - Individual free (for ref_count + state validation)
// ---------------------------------------------------------------------------

/// Per-CPU page cache with an array-based stack.
///
/// Each CPU has its own cache. Access requires a PreemptGuard (pins to CPU).
/// The `count` and `stack` fields are accessed exclusively by the owning CPU
/// (no atomics needed). `alloc_count`/`free_count` are atomic for stats.
#[repr(C, align(64))]
struct PerCpuPageCache {
    /// Stack of cached frame numbers. `stack[0..count]` are valid entries.
    stack: [u32; PCP_CAPACITY],
    /// Number of frames in the stack (stack-top index).
    count: u32,
    /// Cumulative alloc stats (atomic for cross-CPU reads in sysinfo).
    alloc_count: AtomicU32,
    /// Cumulative free stats (atomic for cross-CPU reads in sysinfo).
    free_count: AtomicU32,
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

static PER_CPU_CACHES: CpuLocal<PerCpuPageCache> = {
    const INIT: CacheAligned<PerCpuPageCache> = CacheAligned(PerCpuPageCache::new());
    CpuLocal::new_with([INIT; MAX_CPUS])
};

static PCP_INIT: InitFlag = InitFlag::new();

// Global frame descriptor table — installed once at boot, read lock-free
// by the PCP alloc fast path to update individual descriptors without
// taking the global PAGE_ALLOCATOR lock. The single boot-time install
// publishes both the base pointer and the length atomically.
static FRAME_TABLE: RawTable<PageFrame> = RawTable::empty();

/// Access a frame descriptor by number. Runs `f` against an exclusive
/// borrow of the descriptor; returns `None` if the frame number is out
/// of range or the table has not been installed.
///
/// **Caller's contract:** the frame must logically belong to the caller
/// (e.g. it lives in the caller's PCP cache or was allocated to it). The
/// `RawTable::with_mut` primitive provides the safe surface; callers
/// that already hold the buddy allocator's lock or a `PreemptGuard`
/// satisfy the exclusivity requirement.
#[inline]
fn frame_desc_with<R>(frame_num: u32, f: impl FnOnce(&mut PageFrame) -> R) -> Option<R> {
    FRAME_TABLE.with_mut(frame_num as usize, f)
}

/// Get a reference to the current CPU's page cache. Caller must already
/// hold a `PreemptGuard` so `cpu` stays pinned for the borrow.
#[inline]
fn pcp_cache(cpu: usize) -> &'static mut PerCpuPageCache {
    PER_CPU_CACHES.get_pinned_mut(cpu)
}

#[derive(Default)]
struct PageAllocator {
    total_frames: u32,
    max_supported_frames: u32,
    free_frames: u32,
    allocated_frames: u32,
    free_lists: [u32; (MAX_ORDER as usize) + 1],
    max_order: u32,
}

impl PageAllocator {
    const fn new() -> Self {
        Self {
            total_frames: 0,
            max_supported_frames: 0,
            free_frames: 0,
            allocated_frames: 0,
            free_lists: [INVALID_PAGE_FRAME; (MAX_ORDER as usize) + 1],
            max_order: 0,
        }
    }

    fn phys_to_frame(&self, phys_addr: PhysAddr) -> u32 {
        (phys_addr.as_u64() >> 12) as u32
    }

    fn frame_to_phys(&self, frame_num: u32) -> PhysAddr {
        PhysAddr::new((frame_num as u64) << 12)
    }

    fn is_valid_frame(&self, frame_num: u32) -> bool {
        frame_num < self.total_frames
    }

    /// Borrow descriptor `frame_num`'s mutable handle. Safe because the
    /// caller already holds `&mut self` via the PAGE_ALLOCATOR lock guard
    /// (the lock excludes any other `&mut` into the table).
    fn frame_desc_mut(&mut self, frame_num: u32) -> Option<&mut PageFrame> {
        if !self.is_valid_frame(frame_num) {
            return None;
        }
        FRAME_TABLE.get_mut(frame_num as usize)
    }

    fn frame_region_id(&mut self, frame_num: u32) -> u16 {
        self.frame_desc_mut(frame_num)
            .map(|f| f.region_id)
            .unwrap_or(INVALID_REGION_ID)
    }

    fn order_block_pages(order: u32) -> u32 {
        if order >= 32 {
            panic!("order_block_pages: invalid order {} >= 32", order);
        }
        1u32 << order
    }

    fn flags_to_order(&self, flags: u32) -> u32 {
        let mut requested = (flags & ALLOC_FLAG_ORDER_MASK) >> ALLOC_FLAG_ORDER_SHIFT;
        if requested > self.max_order {
            requested = self.max_order;
        }
        requested
    }

    fn page_state_for_flags(flags: u32) -> u8 {
        if flags & ALLOC_FLAG_DMA != 0 {
            PAGE_FRAME_DMA
        } else if flags & ALLOC_FLAG_KERNEL != 0 {
            PAGE_FRAME_KERNEL
        } else {
            PAGE_FRAME_ALLOCATED
        }
    }

    fn frame_state_is_allocated(state: u8) -> bool {
        matches!(
            state,
            PAGE_FRAME_ALLOCATED | PAGE_FRAME_KERNEL | PAGE_FRAME_DMA | PAGE_FRAME_PCP
        )
    }

    fn free_lists_reset(&mut self) {
        self.free_lists.fill(INVALID_PAGE_FRAME);
    }

    fn free_list_push(&mut self, order: u32, frame_num: u32) {
        let head = self.free_lists[order as usize];
        if let Some(frame) = self.frame_desc_mut(frame_num) {
            frame.next_free = head;
            frame.order = order as u16;
            frame.state = PAGE_FRAME_FREE;
            frame.flags = 0;
            frame.ref_count = 0;
            self.free_lists[order as usize] = frame_num;
        }
    }

    fn free_list_detach(&mut self, order: u32, target_frame: u32) -> bool {
        let mut prev = INVALID_PAGE_FRAME;
        let mut current = self.free_lists[order as usize];

        while current != INVALID_PAGE_FRAME {
            if current == target_frame {
                let next = self
                    .frame_desc_mut(current)
                    .map(|f| f.next_free)
                    .unwrap_or(INVALID_PAGE_FRAME);
                if prev == INVALID_PAGE_FRAME {
                    self.free_lists[order as usize] = next;
                } else if let Some(prev_desc) = self.frame_desc_mut(prev) {
                    prev_desc.next_free = next;
                }
                if let Some(curr_desc) = self.frame_desc_mut(current) {
                    curr_desc.next_free = INVALID_PAGE_FRAME;
                }
                return true;
            }
            prev = current;
            current = self
                .frame_desc_mut(current)
                .map(|f| f.next_free)
                .unwrap_or(INVALID_PAGE_FRAME);
        }

        false
    }

    fn block_meets_flags(&self, frame_num: u32, order: u32, flags: u32) -> bool {
        let phys = self.frame_to_phys(frame_num).as_u64();
        let span = (Self::order_block_pages(order) as u64) * PAGE_SIZE_4KB;
        if flags & ALLOC_FLAG_DMA != 0 && phys + span > DMA_MEMORY_LIMIT {
            return false;
        }
        true
    }

    fn free_list_take_matching(&mut self, order: u32, flags: u32) -> u32 {
        let mut prev = INVALID_PAGE_FRAME;
        let mut current = self.free_lists[order as usize];

        while current != INVALID_PAGE_FRAME {
            if self.block_meets_flags(current, order, flags) {
                let next = self
                    .frame_desc_mut(current)
                    .map(|f| f.next_free)
                    .unwrap_or(INVALID_PAGE_FRAME);
                if prev == INVALID_PAGE_FRAME {
                    self.free_lists[order as usize] = next;
                } else if let Some(prev_desc) = self.frame_desc_mut(prev) {
                    prev_desc.next_free = next;
                }
                if let Some(curr_desc) = self.frame_desc_mut(current) {
                    curr_desc.next_free = INVALID_PAGE_FRAME;
                }

                let pages = Self::order_block_pages(order);
                if self.free_frames >= pages {
                    self.free_frames -= pages;
                }
                return current;
            }

            prev = current;
            current = self
                .frame_desc_mut(current)
                .map(|f| f.next_free)
                .unwrap_or(INVALID_PAGE_FRAME);
        }

        INVALID_PAGE_FRAME
    }

    fn insert_block_coalescing(&mut self, frame_num: u32, order: u32) {
        if !self.is_valid_frame(frame_num) {
            return;
        }

        let mut curr_frame = frame_num;
        let mut curr_order = order;
        let region_id = self.frame_region_id(frame_num);

        while curr_order < self.max_order {
            let buddy = curr_frame ^ Self::order_block_pages(curr_order);
            let buddy_desc = self.frame_desc_mut(buddy);

            let can_merge = buddy_desc
                .map(|b| {
                    b.state == PAGE_FRAME_FREE
                        && b.order == curr_order as u16
                        && b.region_id == region_id
                })
                .unwrap_or(false);
            if !can_merge {
                break;
            }

            if !self.free_list_detach(curr_order, buddy) {
                break;
            }

            curr_frame = curr_frame.min(buddy);
            curr_order += 1;
        }

        self.free_list_push(curr_order, curr_frame);
        self.free_frames += Self::order_block_pages(curr_order);
    }

    fn allocate_block(&mut self, order: u32, flags: u32) -> u32 {
        let mut current_order = order;
        while current_order <= self.max_order {
            let block = self.free_list_take_matching(current_order, flags);
            if block == INVALID_PAGE_FRAME {
                current_order += 1;
                continue;
            }

            while current_order > order {
                current_order -= 1;
                let buddy = block + Self::order_block_pages(current_order);
                self.free_list_push(current_order, buddy);
                self.free_frames += Self::order_block_pages(current_order);
            }

            if let Some(desc) = self.frame_desc_mut(block) {
                desc.ref_count = 1;
                desc.flags = flags as u8;
                desc.order = order as u16;
                desc.state = Self::page_state_for_flags(flags);
            }
            self.allocated_frames += Self::order_block_pages(order);
            return block;
        }

        INVALID_PAGE_FRAME
    }

    fn allocate_batch_for_pcp(&mut self, frames: &mut [u32], flags: u32) -> usize {
        let mut count = 0;
        for slot in frames.iter_mut() {
            let frame_num = self.allocate_block(0, flags);
            if frame_num == INVALID_PAGE_FRAME {
                break;
            }
            if let Some(desc) = self.frame_desc_mut(frame_num) {
                desc.state = PAGE_FRAME_PCP;
            }
            *slot = frame_num;
            count += 1;
        }
        count
    }

    fn free_batch_from_pcp(&mut self, frames: &[u32]) {
        for &frame_num in frames {
            if frame_num == INVALID_PAGE_FRAME {
                continue;
            }
            if let Some(desc) = self.frame_desc_mut(frame_num) {
                if desc.state == PAGE_FRAME_PCP {
                    desc.ref_count = 0;
                    desc.flags = 0;
                    desc.state = PAGE_FRAME_FREE;
                    self.allocated_frames = self.allocated_frames.saturating_sub(1);
                    self.insert_block_coalescing(frame_num, 0);
                }
            }
        }
    }

    fn derive_max_order(total_frames: u32) -> u32 {
        let mut order = 0;
        while order < MAX_ORDER && Self::order_block_pages(order) <= total_frames {
            order += 1;
        }
        order.saturating_sub(1)
    }

    fn seed_region_from_map(&mut self, region: &MmRegion, region_id: u16) {
        if region.kind != MmRegionKind::Usable || region.length == 0 {
            return;
        }

        let mut aligned_start = align_up_u64(region.phys_base, PAGE_SIZE_4KB);
        if aligned_start == 0 {
            aligned_start = PAGE_SIZE_4KB;
        }
        let aligned_end = align_down_u64(region.phys_base + region.length, PAGE_SIZE_4KB);
        if aligned_end <= aligned_start {
            return;
        }

        let mut cursor = aligned_start;
        while cursor < aligned_end {
            let mut next = aligned_end;
            let mut skip_end = 0u64;

            let res_count = mm_reservations_count();
            for idx in 0..res_count {
                let Some(res) = mm_reservations_get(idx) else {
                    continue;
                };
                if res.flags & MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS == 0 {
                    continue;
                }
                let res_start = align_down_u64(res.phys_base, PAGE_SIZE_4KB);
                let res_end = align_up_u64(res.phys_base + res.length, PAGE_SIZE_4KB);
                if res_end <= cursor || res_start >= aligned_end {
                    continue;
                }
                if res_start <= cursor && res_end > cursor {
                    if res_end > skip_end {
                        skip_end = res_end;
                    }
                } else if res_start > cursor && res_start < next {
                    next = res_start;
                }
            }

            if skip_end > cursor {
                cursor = skip_end;
                continue;
            }

            if next > cursor {
                self.seed_range(cursor, next, region_id);
            }
            cursor = next;
        }
    }

    fn seed_range(&mut self, start: u64, end: u64, region_id: u16) {
        let start_frame = self.phys_to_frame(PhysAddr::new(start));
        let mut end_frame = self.phys_to_frame(PhysAddr::new(end));
        if start_frame >= self.total_frames {
            return;
        }
        if end_frame > self.total_frames {
            end_frame = self.total_frames;
        }

        let mut remaining = end_frame - start_frame;
        let mut frame = start_frame;
        let seeded_id = if region_id == INVALID_REGION_ID {
            0
        } else {
            region_id
        };

        while remaining > 0 {
            let mut order = 0;
            while order < self.max_order {
                let block_pages = Self::order_block_pages(order);
                if frame & (block_pages - 1) != 0 {
                    break;
                }
                if block_pages > remaining {
                    break;
                }
                order += 1;
            }
            if order > 0 {
                order -= 1;
            }

            let block_pages = Self::order_block_pages(order);
            for i in 0..block_pages {
                if let Some(f) = self.frame_desc_mut(frame + i) {
                    f.region_id = seeded_id;
                }
            }
            self.insert_block_coalescing(frame, order);
            frame += block_pages;
            remaining -= block_pages;
        }
    }
}

static PAGE_ALLOCATOR: SpinLock<PageAllocator> =
    SpinLock::new(PageAllocator::new(), LOCK_LEVEL_ALLOCATOR);

const DMA_MEMORY_LIMIT: u64 = 0x0100_0000;

#[inline]
fn get_current_cpu() -> usize {
    slopos_arch::pcr::get_current_cpu()
}

/// Pop a single page from the per-CPU cache.
///
/// **Lock-free fast path.** The frame number is popped from the per-CPU
/// stack with only a PreemptGuard. The frame descriptor is updated via the
/// global `FRAMES_PTR` without taking the `PAGE_ALLOCATOR` lock.
///
/// # Safety contract
/// Caller must hold a `PreemptGuard` so `cpu` remains stable. The frame
/// being popped is exclusively owned by this CPU's PCP (state = PCP,
/// ref_count = 0), so updating its descriptor is safe without the lock.
fn pcp_try_alloc(cpu: usize) -> u32 {
    debug_assert!(
        PreemptGuard::is_active(),
        "pcp_try_alloc requires PreemptGuard"
    );

    if cpu >= MAX_CPUS || !PCP_INIT.is_set() {
        return INVALID_PAGE_FRAME;
    }

    let cache = pcp_cache(cpu);

    if cache.count == 0 {
        return INVALID_PAGE_FRAME;
    }

    // Pop from stack — no lock, no atomics.
    cache.count -= 1;
    let frame_num = cache.stack[cache.count as usize];
    cache.stack[cache.count as usize] = INVALID_PAGE_FRAME;
    cache.alloc_count.fetch_add(1, Ordering::Relaxed);

    // Update the frame descriptor to mark it allocated. The frame was in
    // our PCP (state = PAGE_FRAME_PCP, ref_count = 0), so no other CPU or
    // code path references it.
    frame_desc_with(frame_num, |desc| {
        desc.state = PAGE_FRAME_ALLOCATED;
        desc.ref_count = 1;
        desc.next_free = INVALID_PAGE_FRAME;
    });

    frame_num
}

/// Refill the per-CPU cache from the buddy allocator.
///
/// Takes the global `PAGE_ALLOCATOR` lock once, allocates a batch of frames,
/// then pushes them onto the per-CPU stack. The lock is held for the batch
/// allocation only — individual frame insertions into the stack are lock-free.
///
/// # Safety contract
/// Caller must hold a `PreemptGuard` so `cpu` remains stable.
fn pcp_refill(cpu: usize, flags: u32) {
    debug_assert!(
        PreemptGuard::is_active(),
        "pcp_refill requires PreemptGuard"
    );

    if cpu >= MAX_CPUS {
        return;
    }

    let cache = pcp_cache(cpu);

    // Skip refill if cache is reasonably full.
    if cache.count >= PCP_LOW_WATERMARK {
        return;
    }

    let mut batch = [INVALID_PAGE_FRAME; PCP_BATCH_SIZE as usize];

    // Single lock acquisition: allocate a batch from the buddy.
    let mut alloc = PAGE_ALLOCATOR.lock();

    // Re-check under lock in case another path changed count.
    if cache.count >= PCP_HIGH_WATERMARK {
        return;
    }
    let needed = PCP_BATCH_SIZE.min(PCP_HIGH_WATERMARK - cache.count);
    let allocated = alloc.allocate_batch_for_pcp(&mut batch[..needed as usize], flags);

    // Mark each frame as PCP and push onto the stack while still under lock.
    // We mark descriptors here (under lock) so the frame state is consistent
    // before the lock is released.
    for i in 0..allocated {
        let frame_num = batch[i];
        if let Some(desc) = alloc.frame_desc_mut(frame_num) {
            desc.state = PAGE_FRAME_PCP;
            desc.ref_count = 0;
            desc.next_free = INVALID_PAGE_FRAME;
        }
        // Push onto stack.
        if (cache.count as usize) < PCP_CAPACITY {
            cache.stack[cache.count as usize] = frame_num;
            cache.count += 1;
        }
    }
}

/// Drain all per-CPU PCP caches back into the buddy allocator.
///
/// Iterates every CPU's stack and returns cached frames to the buddy via
/// [`free_batch_from_pcp`], batching up to [`PCP_BATCH_SIZE`] frames per
/// lock acquisition.
///
/// # Safety
/// Must only be called during system shutdown when no concurrent PCP
/// operations or per-CPU allocations are in progress.
pub fn pcp_drain_all() {
    PER_CPU_CACHES.for_each_mut_at_shutdown(|_cpu, cache| {
        let mut batch = [INVALID_PAGE_FRAME; PCP_BATCH_SIZE as usize];

        loop {
            if cache.count == 0 {
                break;
            }

            let mut drained = 0usize;
            {
                let mut alloc = PAGE_ALLOCATOR.lock();
                while drained < PCP_BATCH_SIZE as usize && cache.count > 0 {
                    cache.count -= 1;
                    let frame_num = cache.stack[cache.count as usize];
                    cache.stack[cache.count as usize] = INVALID_PAGE_FRAME;
                    batch[drained] = frame_num;
                    drained += 1;
                }
                if drained > 0 {
                    alloc.free_batch_from_pcp(&batch[..drained]);
                }
            }

            if drained == 0 {
                break;
            }
        }
    });
}

pub fn init_page_allocator(frame_array: *mut c_void, max_frames: u32) -> c_int {
    if frame_array.is_null() || max_frames == 0 {
        panic!("init_page_allocator: Invalid parameters");
    }

    // Install the boot-allocated frame descriptor table once. Safe via
    // `RawTable::install`: the slice carries the bootloader-published
    // backing store with `'static` lifetime; the install hook publishes
    // base + length atomically so concurrent readers see either fully
    // empty or fully populated state.
    if !FRAME_TABLE.is_installed() {
        let frames_ptr = frame_array as *mut PageFrame;
        // The caller (memory_init) has just allocated and mapped
        // `max_frames` PageFrame slots starting at `frame_array` with
        // `'static` lifetime; we hold the only reference and publish
        // ownership exactly once via `RawTable::install`.
        let slice: &'static mut [PageFrame] =
            slopos_ostd::util::ptr_buf::borrow_buf_mut(frames_ptr, max_frames as usize);
        FRAME_TABLE.install(slice);
    }

    let mut alloc = PAGE_ALLOCATOR.lock();
    alloc.total_frames = max_frames;
    alloc.max_supported_frames = max_frames;
    alloc.free_frames = 0;
    alloc.allocated_frames = 0;
    alloc.max_order = PageAllocator::derive_max_order(max_frames);
    alloc.free_lists_reset();

    for i in 0..max_frames {
        if let Some(frame) = alloc.frame_desc_mut(i) {
            frame.ref_count = 0;
            frame.state = PAGE_FRAME_RESERVED;
            frame.flags = 0;
            frame.order = 0;
            frame.region_id = INVALID_REGION_ID;
            frame.next_free = INVALID_PAGE_FRAME;
        }
    }

    klog_debug!(
        "Page frame allocator initialized with {} frame descriptors (max order {})",
        max_frames,
        alloc.max_order
    );

    0
}

pub fn finalize_page_allocator() -> c_int {
    let mut alloc = PAGE_ALLOCATOR.lock();
    alloc.free_lists_reset();
    alloc.free_frames = 0;
    alloc.allocated_frames = 0;

    let region_count = mm_region_count();
    for i in 0..region_count {
        if let Some(region) = mm_region_get(i) {
            alloc.seed_region_from_map(&region, i as u16);
        }
    }

    drop(alloc);

    PCP_INIT.mark_set();

    let alloc = PAGE_ALLOCATOR.lock();
    klog_info!(
        "Page allocator ready: {} pages available (PCP enabled)",
        alloc.free_frames
    );

    0
}

/// Raw multi-page buddy allocator entry point — bootstrap escape and
/// policy-flag opt-out (NO_PCP / DMA). Not part of the supported public
/// surface: every new caller should reach for the typestate
/// `Frame::<KernelMeta>::alloc` (single page) or `alloc_kernel_pages`
/// (multi-page legacy bridge) instead. The only legitimate consumers
/// of this function in tree are:
///
/// 1. `kernel_meta::install_meta_slots` — bootstrapping `META_SLOTS`
///    itself; the typestate path physically cannot work yet.
/// 2. `frame_alloc_shim::LegacyFrameAllocShim::alloc` — the typestate
///    backend (the typestate calls into here, not the other way round).
/// 3. Callers that need `ALLOC_FLAG_NO_PCP` or `ALLOC_FLAG_DMA` (test
///    suites + the xe driver's MMIO buffer + memfd's multi-page user
///    backing). These will migrate to a future `FrameAllocOptions`
///    policy axis; until then they remain on the raw escape.
#[doc(hidden)]
pub fn __alloc_page_frames_raw(count: u32, flags: u32) -> PhysAddr {
    if count == 0 {
        return PhysAddr::NULL;
    }

    let mut order = 0;
    let mut pages = 1;
    while pages < count && order < MAX_ORDER {
        pages <<= 1;
        order += 1;
    }

    let use_pcp = order == 0
        && (flags & ALLOC_FLAG_DMA) == 0
        && (flags & ALLOC_FLAG_KERNEL) == 0
        && (flags & ALLOC_FLAG_ORDER_MASK) == 0
        && (flags & ALLOC_FLAG_NO_PCP) == 0
        && PCP_INIT.is_set();

    let mut attempts = 0u32;
    loop {
        let frame_num = if use_pcp {
            // PreemptGuard pins us to this CPU for the duration of PCP
            // operations, preventing migration races that could allow two
            // CPUs to access the same per-CPU cache concurrently.
            let _no_migrate = PreemptGuard::new();
            let cpu = get_current_cpu();

            let mut frame = pcp_try_alloc(cpu);

            if frame == INVALID_PAGE_FRAME {
                pcp_refill(cpu, flags);
                frame = pcp_try_alloc(cpu);
            }

            if frame == INVALID_PAGE_FRAME {
                let mut alloc = PAGE_ALLOCATOR.lock();
                let flag_order = alloc.flags_to_order(flags);
                let actual_order = flag_order.max(order);
                frame = alloc.allocate_block(actual_order, flags);
            }

            frame
        } else {
            let mut alloc = PAGE_ALLOCATOR.lock();
            let flag_order = alloc.flags_to_order(flags);
            if flag_order > order {
                order = flag_order;
            }
            alloc.allocate_block(order, flags)
        };

        if frame_num == INVALID_PAGE_FRAME {
            klog_info!("__alloc_page_frames_raw: No suitable block available");
            return PhysAddr::NULL;
        }

        let phys_addr = {
            let alloc = PAGE_ALLOCATOR.lock();
            alloc.frame_to_phys(frame_num)
        };

        // Always zero. The bug class behind `0xdfdedddcdbdad9d8`-shape
        // wild RIPs was a freed page that kept its `(i & 0xFF) as u8`
        // test pattern, was reused as kernel/user stack-or-control
        // memory, and a subsequent `ret` decoded those bytes as a
        // function address. The buddy unconditionally scrubs; the
        // typestate `Frame<_, Uninit>` is a type-level audit point
        // (caller still has to scrub before promoting to `Zeroed`)
        // but no longer represents a runtime perf escape.
        {
            let span_pages = if use_pcp {
                1
            } else {
                PageAllocator::order_block_pages(order)
            };
            let mut ok = true;
            for i in 0..span_pages {
                let page_phys = phys_addr.offset(i as u64 * PAGE_SIZE_4KB);
                if zero_physical_page(page_phys) != 0 {
                    klog_info!(
                        "__alloc_page_frames_raw: Failed to zero page at phys 0x{:x}",
                        page_phys.as_u64()
                    );
                    ok = false;
                    break;
                }
            }
            if !ok {
                // Keep the frame allocated to avoid reuse of bad pages.
                attempts += 1;
                if attempts > 64 {
                    return PhysAddr::NULL;
                }
                continue;
            }
        }

        return phys_addr;
    }
}

/// Typestate-checked single-page kernel allocation with caller-supplied
/// [`FrameAllocOptions`]. Lets the caller toggle policy bits
/// (`no_pcp`, `dma`) without dropping back to the raw `__*_raw` API.
/// Returns the raw `PhysAddr` for handoff to legacy free paths;
/// internals match [`alloc_kernel_page`].
pub fn alloc_kernel_page_with(opts: slopos_ostd::mm::frame::FrameAllocOptions) -> PhysAddr {
    use slopos_ostd::mm::frame::{Frame, KernelMeta};
    Frame::<KernelMeta>::alloc(opts).map_or(PhysAddr::NULL, |f| {
        // SAFETY: see `alloc_kernel_page`.
        unsafe { f.into_phys_release() }
    })
}

/// Typestate-checked multi-page kernel allocation with caller-supplied
/// [`FrameAllocOptions`]. The `count` argument overrides
/// `opts.size_pages`. See [`alloc_kernel_page_with`].
pub fn alloc_kernel_pages_with(
    count: u32,
    opts: slopos_ostd::mm::frame::FrameAllocOptions,
) -> PhysAddr {
    use slopos_ostd::mm::frame::{Frame, KernelMeta};
    if count == 0 {
        return PhysAddr::NULL;
    }
    let opts = slopos_ostd::mm::frame::FrameAllocOptions {
        size_pages: count as usize,
        ..opts
    };
    Frame::<KernelMeta>::alloc(opts).map_or(PhysAddr::NULL, |f| {
        // SAFETY: see `alloc_kernel_page`.
        unsafe { f.into_phys_release() }
    })
}

/// Typestate-checked multi-page kernel allocation. Routes through
/// [`slopos_ostd::mm::frame::Frame<KernelMeta, Zeroed>::alloc`] with
/// `size_pages = count`, releases the leading `MetaSlot` to `UNUSED`,
/// and returns the raw paddr so legacy `free_page_frame` callers
/// continue to work unchanged. See [`alloc_kernel_page`].
pub fn alloc_kernel_pages(count: u32) -> PhysAddr {
    use slopos_ostd::mm::frame::{Frame, FrameAllocOptions, KernelMeta};
    if count == 0 {
        return PhysAddr::NULL;
    }
    let opts = FrameAllocOptions {
        size_pages: count as usize,
        ..FrameAllocOptions::single()
    };
    Frame::<KernelMeta>::alloc(opts).map_or(PhysAddr::NULL, |f| {
        // SAFETY: see `alloc_kernel_page`.
        unsafe { f.into_phys_release() }
    })
}

/// Typestate-checked single-page kernel allocation. Routes through
/// [`slopos_ostd::mm::frame::Frame<KernelMeta, Zeroed>::alloc`] so
/// the page is guaranteed zeroed (the typestate refuses any other
/// state without an explicit `unsafe` opt-out) and then releases the
/// `MetaSlot` to `UNUSED` before returning the raw paddr.
///
/// Migration target for legacy `alloc_page_frame(0)` call sites:
/// the alloc goes through the typestate gate, the resulting `PhysAddr`
/// stays compatible with today's `free_page_frame` free path. New
/// code should hold a `Frame<KernelMeta>` directly instead of routing
/// through this bridge.
pub fn alloc_kernel_page() -> PhysAddr {
    use slopos_ostd::mm::frame::{Frame, FrameAllocOptions, KernelMeta};
    Frame::<KernelMeta>::alloc(FrameAllocOptions::single()).map_or(PhysAddr::NULL, |f| {
        // SAFETY: we immediately surrender the typed Frame to the
        // legacy raw-paddr free path; the caller of this wrapper
        // owns the dealloc obligation via `free_page_frame`.
        unsafe { f.into_phys_release() }
    })
}

/// Raw single-page buddy allocator entry point. See
/// [`__alloc_page_frames_raw`] for the audit-point rationale; same
/// rules apply.
#[doc(hidden)]
pub fn __alloc_page_frame_raw(flags: u32) -> PhysAddr {
    let phys = __alloc_page_frames_raw(1, flags);
    // LUF reuse-drain hook: if the frame we're about to hand out is
    // still referenced by a deferred TLB flush on this CPU, drain the
    // queue before the new owner installs its own mapping. Missing
    // this would let a fresh translation for the same `phys` race a
    // stale non-global TLB entry belonging to the previous owner.
    if !phys.is_null() {
        crate::mmu::luf::drain_if_reusing_frame(phys);
    }
    phys
}

/// Batch-allocate up to [`PCP_CAPACITY`] order-0 frames under a single
/// [`PreemptGuard`].
///
/// Designed for callers that need several contiguous (in time, not phys)
/// single pages — typically a kernel stack's backing frames. Holding one
/// guard across the whole batch amortises the PCP bookkeeping and the
/// address-translation lock. PCP misses fall back to the global buddy
/// allocator transparently.
///
/// Returns the number of slots in `out` that now contain a valid
/// [`PhysAddr`]. On short return, the caller is responsible for freeing
/// whatever they got and deciding whether to retry with a different
/// allocation strategy (e.g. larger-order block).
pub fn alloc_page_frames_pcp_batch(out: &mut [PhysAddr]) -> usize {
    if out.is_empty() {
        return 0;
    }
    if out.len() > PCP_CAPACITY {
        // Exceeds per-CPU cache capacity; fall back to per-page calls so
        // we don't silently short-return for pathological requests.
        let mut filled = 0usize;
        for slot in out.iter_mut() {
            let pa = __alloc_page_frame_raw(0);
            if pa.is_null() {
                break;
            }
            *slot = pa;
            filled += 1;
        }
        return filled;
    }

    let mut frames = [INVALID_PAGE_FRAME; PCP_CAPACITY];
    let mut filled = 0usize;

    if PCP_INIT.is_set() {
        let _no_migrate = PreemptGuard::new();
        let cpu = get_current_cpu();
        while filled < out.len() {
            let mut frame = pcp_try_alloc(cpu);
            if frame == INVALID_PAGE_FRAME {
                pcp_refill(cpu, 0);
                frame = pcp_try_alloc(cpu);
                if frame == INVALID_PAGE_FRAME {
                    break;
                }
            }
            frames[filled] = frame;
            filled += 1;
        }
    }

    // Anything the PCP couldn't satisfy goes through the global buddy.
    while filled < out.len() {
        let frame_num = {
            let mut alloc = PAGE_ALLOCATOR.lock();
            alloc.allocate_block(0, 0)
        };
        if frame_num == INVALID_PAGE_FRAME {
            break;
        }
        frames[filled] = frame_num;
        filled += 1;
    }

    if filled > 0 {
        let alloc = PAGE_ALLOCATOR.lock();
        for i in 0..filled {
            out[i] = alloc.frame_to_phys(frames[i]);
        }
        drop(alloc);
        // Zero each frame for the same reason `alloc_page_frames` does:
        // pages are zero by default. `alloc_page_frames_pcp_batch` does
        // not currently expose a `NO_INIT` opt-out — its sole caller
        // (`KernelStack::new`) already overwrites the entire stack via
        // `zero_stack_pages`, so the redundant scrub is acceptable for
        // now. Refactor to a flagged variant if a perf-critical caller
        // appears.
        for i in 0..filled {
            if zero_physical_page(out[i]) != 0 {
                klog_info!(
                    "alloc_page_frames_pcp_batch: zero_physical_page failed at 0x{:x}",
                    out[i].as_u64()
                );
            }
        }
    }
    filled
}

pub fn free_page_frame(phys_addr: PhysAddr) -> c_int {
    // Pin to CPU for safe PCP access and do ALL state checks + transitions
    // under a single lock acquisition to eliminate TOCTOU races where two
    // concurrent free_page_frame calls on the same frame could both see
    // ref_count==1 and double-free.
    let _no_migrate = PreemptGuard::new();
    let cpu = get_current_cpu();

    let mut alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    if !alloc.is_valid_frame(frame_num) {
        return -1;
    }

    let Some(frame) = alloc.frame_desc_mut(frame_num) else {
        return -1;
    };
    if !PageAllocator::frame_state_is_allocated(frame.state) {
        return 0;
    }
    // A frame already parked in PCP (state == PAGE_FRAME_PCP) must not
    // fall through to the buddy free path — treat as a no-op.
    if frame.state == PAGE_FRAME_PCP {
        return 0;
    }
    if frame.ref_count > 1 {
        frame.ref_count -= 1;
        return 0;
    }

    let order = frame.order as u32;
    let is_pcp_candidate = order == 0 && frame.state == PAGE_FRAME_ALLOCATED && PCP_INIT.is_set();

    if is_pcp_candidate {
        let cache = pcp_cache(cpu);
        if cache.count < PCP_HIGH_WATERMARK {
            // Mark the frame as PCP while holding the lock for consistency.
            if let Some(desc) = alloc.frame_desc_mut(frame_num) {
                desc.state = PAGE_FRAME_PCP;
                desc.ref_count = 0;
                desc.next_free = INVALID_PAGE_FRAME;
            }

            // Push onto per-CPU stack.
            cache.stack[cache.count as usize] = frame_num;
            cache.count += 1;
            cache.free_count.fetch_add(1, Ordering::Relaxed);

            // Drain if over watermark.
            if cache.count > PCP_HIGH_WATERMARK {
                let to_drain = (cache.count - PCP_HIGH_WATERMARK / 2).min(PCP_BATCH_SIZE) as usize;
                let mut batch = [INVALID_PAGE_FRAME; PCP_BATCH_SIZE as usize];
                let mut drained = 0usize;
                while drained < to_drain && cache.count > 0 {
                    cache.count -= 1;
                    batch[drained] = cache.stack[cache.count as usize];
                    cache.stack[cache.count as usize] = INVALID_PAGE_FRAME;
                    drained += 1;
                }
                if drained > 0 {
                    alloc.free_batch_from_pcp(&batch[..drained]);
                }
            }
            return 0;
        }
    }

    // Fallback: return directly to buddy allocator.
    if let Some(frame) = alloc.frame_desc_mut(frame_num) {
        let pages = PageAllocator::order_block_pages(order);
        frame.ref_count = 0;
        frame.flags = 0;
        frame.state = PAGE_FRAME_FREE;
        alloc.allocated_frames = alloc.allocated_frames.saturating_sub(pages);
        alloc.insert_block_coalescing(frame_num, order);
    }

    0
}

pub fn page_allocator_descriptor_size() -> usize {
    core::mem::size_of::<PageFrame>()
}

pub fn page_allocator_max_supported_frames() -> u32 {
    PAGE_ALLOCATOR.lock().max_supported_frames
}

pub fn get_page_allocator_stats(total: *mut u32, free: *mut u32, allocated: *mut u32) {
    let alloc = PAGE_ALLOCATOR.lock();

    // PCP-cached frames are allocated from the buddy but not in use —
    // include them in the free count for accurate statistics.
    let mut pcp_count = 0u32;
    for cpu in 0..MAX_CPUS {
        if let Some(cache) = PER_CPU_CACHES.snapshot_for_cpu(cpu) {
            pcp_count = pcp_count.saturating_add(cache.count);
        }
    }

    write_optional_u32(total, alloc.total_frames);
    write_optional_u32(free, alloc.free_frames.saturating_add(pcp_count));
    write_optional_u32(allocated, alloc.allocated_frames.saturating_sub(pcp_count));
}

pub fn get_pcp_stats(cpu: usize, count: *mut u32, allocs: *mut u32, frees: *mut u32) {
    if cpu >= MAX_CPUS {
        return;
    }

    let Some(cache) = PER_CPU_CACHES.snapshot_for_cpu(cpu) else {
        return;
    };
    write_optional_u32(count, cache.count);
    write_optional_u32(allocs, cache.alloc_count.load(Ordering::Relaxed));
    write_optional_u32(frees, cache.free_count.load(Ordering::Relaxed));
}

/// Write `value` through `out` if non-null. Used for `*mut u32` C-ABI
/// shim outputs.
#[inline]
fn write_optional_u32(out: *mut u32, value: u32) {
    if out.is_null() {
        return;
    }
    // SAFETY: out is non-null per the check above; caller-supplied
    // C-ABI output slot.
    unsafe { *out = value };
}

pub fn page_frame_is_tracked(phys_addr: PhysAddr) -> c_int {
    let alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    (frame_num < alloc.total_frames) as c_int
}

pub fn page_frame_can_free(phys_addr: PhysAddr) -> c_int {
    let mut alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    if !alloc.is_valid_frame(frame_num) {
        return 0;
    }
    let Some(frame) = alloc.frame_desc_mut(frame_num) else {
        return 0;
    };
    PageAllocator::frame_state_is_allocated(frame.state) as c_int
}

pub fn page_frame_inc_ref(phys_addr: PhysAddr) -> c_int {
    let mut alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    if !alloc.is_valid_frame(frame_num) {
        return -1;
    }
    let Some(frame) = alloc.frame_desc_mut(frame_num) else {
        return -1;
    };
    if !PageAllocator::frame_state_is_allocated(frame.state) {
        return -1;
    }
    frame.ref_count = frame.ref_count.saturating_add(1);
    frame.ref_count as c_int
}

pub fn page_frame_get_ref(phys_addr: PhysAddr) -> u32 {
    let mut alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    if !alloc.is_valid_frame(frame_num) {
        return 0;
    }
    let Some(frame) = alloc.frame_desc_mut(frame_num) else {
        return 0;
    };
    frame.ref_count
}

pub fn page_allocator_paint_all(value: u8) {
    let alloc = PAGE_ALLOCATOR.lock();
    if !FRAME_TABLE.is_installed() {
        return;
    }

    for frame_num in 0..alloc.total_frames {
        let phys_addr = alloc.frame_to_phys(frame_num);
        if let Some(virt_addr) = phys_addr.to_virt_checked() {
            paint_page_at_virt(virt_addr.as_mut_ptr::<u8>(), value);
        }
    }
}

#[inline]
fn paint_page_at_virt(ptr: *mut u8, value: u8) {
    if ptr.is_null() {
        return;
    }
    // Caller has resolved `ptr` from a valid `PhysAddr` whose HHDM
    // mapping is live; the page is exclusively owned for the duration
    // of the write.
    slopos_ostd::util::ptr_buf::borrow_buf_mut(ptr, PAGE_SIZE_4KB as usize).fill(value);
}

fn zero_physical_page(phys_addr: PhysAddr) -> c_int {
    if phys_addr.is_null() {
        return -1;
    }

    match phys_addr.to_virt_checked() {
        Some(virt) => {
            paint_page_at_virt(virt.as_mut_ptr::<u8>(), 0);
            0
        }
        None => -1,
    }
}

// =============================================================================
// OwnedPageFrame - RAII wrapper for automatic page deallocation
// =============================================================================

/// Owning handle to a single 4 KiB kernel-owned physical frame.
///
/// Aliased onto `slopos_ostd::mm::frame::Frame<KernelMeta>` so the
/// underlying ref-counted slot machinery from OSTD drives the
/// allocate/free lifecycle. The kernel-side allocator is registered
/// through [`crate::frame_alloc_shim`], and the final
/// [`slopos_ostd::mm::frame::Frame`] drop routes back into
/// [`free_page_frame`] via OSTD's `KernelMeta::on_drop`.
pub type OwnedPageFrame = slopos_ostd::mm::frame::Frame<crate::kernel_meta::KernelMeta>;

/// Synonym kept for callers that prefer the OSTD-flavoured name.
pub use OwnedPageFrame as KernelFrame;
