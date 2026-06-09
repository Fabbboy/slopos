//! Buddy allocator core for the kernel's physical page frames.
//!
//! [`BuddyAllocator`] is the safe-Rust [`FrameAlloc`] implementation
//! that OSTD's [`crate::page_alloc::frame_alloc_handle`] registers via
//! [`slopos_ostd::mm::frame_alloc::register_frame_allocator`]. The
//! single static instance lives in [`super`]; this module exposes the
//! type and the orchestration logic.
//!
//! The allocator owns three pieces of state:
//!
//! 1. [`BuddyInner`] — the per-order free-lists and frame counters,
//!    guarded by a [`SpinLock`] at [`LOCK_LEVEL_ALLOCATOR`].
//! 2. [`RawTable<PageFrame>`] — the page-descriptor table, a flat
//!    array indexed by frame number. Access is gated either by the
//!    lock above (when mutating free-list links) or by exclusive PCP
//!    ownership (when a frame is parked in a per-CPU cache and the
//!    holder pins itself with a [`PreemptGuard`]).
//! 3. `state: AtomicU8` — explicit boot lifecycle:
//!    `Uninit → Sized → Seeded → Live`. Replaces today's loose
//!    `PCP_INIT` flag with a named enum so transitions are visible at
//!    the call sites and `debug_assert!`-checked at the
//!    method boundary.

use core::sync::atomic::{AtomicU8, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::frame::{FrameAlloc, FrameAllocOptions, Paddr};
use slopos_ostd::sync::{LOCK_LEVEL_ALLOCATOR, PreemptGuard, RawTable, SpinLock};
use slopos_ostd::{align_down_u64, align_up_u64, klog_debug, klog_info};

use crate::hhdm::PhysAddrHhdm;
use crate::memory_reservations::{
    MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS, MmRegion, MmRegionKind, mm_region_count, mm_region_get,
    mm_reservations_count, mm_reservations_get,
};
use crate::paging_defs::PAGE_SIZE_4KB;

use super::pcp;

// ---------------------------------------------------------------------------
// Legacy public constants (preserved verbatim — external callers still set
// these directly through `__alloc_page_frame_raw`).
// ---------------------------------------------------------------------------

pub const ALLOC_FLAG_DMA: u32 = 0x02;
pub const ALLOC_FLAG_KERNEL: u32 = 0x04;
pub const ALLOC_FLAG_ORDER_SHIFT: u32 = 8;
pub const ALLOC_FLAG_ORDER_MASK: u32 = 0x1F << ALLOC_FLAG_ORDER_SHIFT;
pub const ALLOC_FLAG_NO_PCP: u32 = 0x80;

// ---------------------------------------------------------------------------
// Descriptor state codes.
// ---------------------------------------------------------------------------

pub(super) const PAGE_FRAME_FREE: u8 = 0x00;
pub(super) const PAGE_FRAME_ALLOCATED: u8 = 0x01;
pub(super) const PAGE_FRAME_RESERVED: u8 = 0x02;
pub(super) const PAGE_FRAME_KERNEL: u8 = 0x03;
pub(super) const PAGE_FRAME_DMA: u8 = 0x04;
pub(super) const PAGE_FRAME_PCP: u8 = 0x05;
pub(super) const PAGE_FRAME_NEVER_REUSE: u8 = 0x06;

pub(super) const INVALID_PAGE_FRAME: u32 = 0xFFFF_FFFF;
pub(super) const MAX_ORDER: u32 = 24;
const INVALID_REGION_ID: u16 = 0xFFFF;
const DMA_MEMORY_LIMIT: u64 = 0x0100_0000;

// ---------------------------------------------------------------------------
// Lifecycle.
// ---------------------------------------------------------------------------

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lifecycle {
    Uninit = 0,
    Sized = 1,
    Seeded = 2,
    Live = 3,
}

impl Lifecycle {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Lifecycle::Uninit,
            1 => Lifecycle::Sized,
            2 => Lifecycle::Seeded,
            3 => Lifecycle::Live,
            _ => Lifecycle::Uninit,
        }
    }
}

// ---------------------------------------------------------------------------
// Descriptor type (one entry per physical frame).
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct PageFrame {
    pub(super) ref_count: u32,
    pub(super) state: u8,
    pub(super) flags: u8,
    pub(super) order: u16,
    pub(super) region_id: u16,
    pub(super) next_free: u32,
}

// ---------------------------------------------------------------------------
// Lock-protected buddy state.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(super) struct BuddyInner {
    pub(super) total_frames: u32,
    pub(super) max_supported_frames: u32,
    pub(super) free_frames: u32,
    pub(super) allocated_frames: u32,
    pub(super) free_lists: [u32; (MAX_ORDER as usize) + 1],
    pub(super) max_order: u32,
}

impl BuddyInner {
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

    pub(super) fn phys_to_frame(&self, phys_addr: PhysAddr) -> u32 {
        (phys_addr.as_u64() >> 12) as u32
    }

    pub(super) fn frame_to_phys(&self, frame_num: u32) -> PhysAddr {
        PhysAddr::new((frame_num as u64) << 12)
    }

    pub(super) fn is_valid_frame(&self, frame_num: u32) -> bool {
        frame_num < self.total_frames
    }

    pub(super) fn frame_desc_mut<'a>(
        &self,
        table: &'a RawTable<PageFrame>,
        frame_num: u32,
    ) -> Option<&'a mut PageFrame> {
        if !self.is_valid_frame(frame_num) {
            return None;
        }
        table.get_mut(frame_num as usize)
    }

    fn frame_region_id(&self, table: &RawTable<PageFrame>, frame_num: u32) -> u16 {
        self.frame_desc_mut(table, frame_num)
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

    pub(super) fn frame_state_is_allocated(state: u8) -> bool {
        matches!(
            state,
            PAGE_FRAME_ALLOCATED | PAGE_FRAME_KERNEL | PAGE_FRAME_DMA | PAGE_FRAME_PCP
        )
    }

    fn free_list_push(&mut self, table: &RawTable<PageFrame>, order: u32, frame_num: u32) {
        let head = self.free_lists[order as usize];
        if let Some(frame) = self.frame_desc_mut(table, frame_num) {
            frame.next_free = head;
            frame.order = order as u16;
            frame.state = PAGE_FRAME_FREE;
            frame.flags = 0;
            frame.ref_count = 0;
            self.free_lists[order as usize] = frame_num;
        }
    }

    fn free_list_detach(
        &mut self,
        table: &RawTable<PageFrame>,
        order: u32,
        target_frame: u32,
    ) -> bool {
        let mut prev = INVALID_PAGE_FRAME;
        let mut current = self.free_lists[order as usize];

        while current != INVALID_PAGE_FRAME {
            if current == target_frame {
                let next = self
                    .frame_desc_mut(table, current)
                    .map(|f| f.next_free)
                    .unwrap_or(INVALID_PAGE_FRAME);
                if prev == INVALID_PAGE_FRAME {
                    self.free_lists[order as usize] = next;
                } else if let Some(prev_desc) = self.frame_desc_mut(table, prev) {
                    prev_desc.next_free = next;
                }
                if let Some(curr_desc) = self.frame_desc_mut(table, current) {
                    curr_desc.next_free = INVALID_PAGE_FRAME;
                }
                return true;
            }
            prev = current;
            current = self
                .frame_desc_mut(table, current)
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

    fn free_list_take_matching(
        &mut self,
        table: &RawTable<PageFrame>,
        order: u32,
        flags: u32,
    ) -> u32 {
        let mut prev = INVALID_PAGE_FRAME;
        let mut current = self.free_lists[order as usize];

        while current != INVALID_PAGE_FRAME {
            if self.block_meets_flags(current, order, flags) {
                let next = self
                    .frame_desc_mut(table, current)
                    .map(|f| f.next_free)
                    .unwrap_or(INVALID_PAGE_FRAME);
                if prev == INVALID_PAGE_FRAME {
                    self.free_lists[order as usize] = next;
                } else if let Some(prev_desc) = self.frame_desc_mut(table, prev) {
                    prev_desc.next_free = next;
                }
                if let Some(curr_desc) = self.frame_desc_mut(table, current) {
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
                .frame_desc_mut(table, current)
                .map(|f| f.next_free)
                .unwrap_or(INVALID_PAGE_FRAME);
        }
        INVALID_PAGE_FRAME
    }

    fn insert_block_coalescing(&mut self, table: &RawTable<PageFrame>, frame_num: u32, order: u32) {
        if !self.is_valid_frame(frame_num) {
            return;
        }

        let mut curr_frame = frame_num;
        let mut curr_order = order;
        let region_id = self.frame_region_id(table, frame_num);

        while curr_order < self.max_order {
            let buddy = curr_frame ^ Self::order_block_pages(curr_order);
            let buddy_desc = self.frame_desc_mut(table, buddy);

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

            if !self.free_list_detach(table, curr_order, buddy) {
                break;
            }

            curr_frame = curr_frame.min(buddy);
            curr_order += 1;
        }

        self.free_list_push(table, curr_order, curr_frame);
        self.free_frames += Self::order_block_pages(curr_order);
    }

    fn allocate_block(&mut self, table: &RawTable<PageFrame>, order: u32, flags: u32) -> u32 {
        let mut current_order = order;
        while current_order <= self.max_order {
            let block = self.free_list_take_matching(table, current_order, flags);
            if block == INVALID_PAGE_FRAME {
                current_order += 1;
                continue;
            }

            while current_order > order {
                current_order -= 1;
                let buddy = block + Self::order_block_pages(current_order);
                self.free_list_push(table, current_order, buddy);
                self.free_frames += Self::order_block_pages(current_order);
            }

            if let Some(desc) = self.frame_desc_mut(table, block) {
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

    fn allocate_batch_for_pcp(
        &mut self,
        table: &RawTable<PageFrame>,
        frames: &mut [u32],
        flags: u32,
    ) -> usize {
        let mut count = 0;
        for slot in frames.iter_mut() {
            let frame_num = self.allocate_block(table, 0, flags);
            if frame_num == INVALID_PAGE_FRAME {
                break;
            }
            if let Some(desc) = self.frame_desc_mut(table, frame_num) {
                desc.state = PAGE_FRAME_PCP;
            }
            *slot = frame_num;
            count += 1;
        }
        count
    }

    fn free_batch_from_pcp(&mut self, table: &RawTable<PageFrame>, frames: &[u32]) {
        for &frame_num in frames {
            if frame_num == INVALID_PAGE_FRAME {
                continue;
            }
            if let Some(desc) = self.frame_desc_mut(table, frame_num) {
                if desc.state == PAGE_FRAME_PCP {
                    desc.ref_count = 0;
                    desc.flags = 0;
                    desc.state = PAGE_FRAME_FREE;
                    self.allocated_frames = self.allocated_frames.saturating_sub(1);
                    self.insert_block_coalescing(table, frame_num, 0);
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

    fn seed_region_from_map(
        &mut self,
        table: &RawTable<PageFrame>,
        region: &MmRegion,
        region_id: u16,
    ) {
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
                self.seed_range(table, cursor, next, region_id);
            }
            cursor = next;
        }
    }

    fn seed_range(&mut self, table: &RawTable<PageFrame>, start: u64, end: u64, region_id: u16) {
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
                if let Some(f) = self.frame_desc_mut(table, frame + i) {
                    f.region_id = seeded_id;
                }
            }
            self.insert_block_coalescing(table, frame, order);
            frame += block_pages;
            remaining -= block_pages;
        }
    }
}

// ---------------------------------------------------------------------------
// BuddyAllocator: the static `FrameAlloc` implementation.
// ---------------------------------------------------------------------------

pub struct BuddyAllocator {
    inner: SpinLock<BuddyInner>,
    frame_table: RawTable<PageFrame>,
    state: AtomicU8,
}

impl BuddyAllocator {
    /// Const constructor for the BSS-resident singleton in
    /// [`super::BUDDY_ALLOCATOR`]. Caller must drive the
    /// `Uninit → Sized → Seeded → Live` lifecycle before the OSTD
    /// frame-allocator registration site reads from it.
    pub const fn new_uninit() -> Self {
        Self {
            inner: SpinLock::new(BuddyInner::new(), LOCK_LEVEL_ALLOCATOR),
            frame_table: RawTable::empty(),
            state: AtomicU8::new(Lifecycle::Uninit as u8),
        }
    }

    fn lifecycle(&self) -> Lifecycle {
        Lifecycle::from_u8(self.state.load(Ordering::Acquire))
    }

    fn advance(&self, from: Lifecycle, to: Lifecycle) {
        let prev = self.state.swap(to as u8, Ordering::AcqRel);
        if prev != from as u8 {
            panic!(
                "BuddyAllocator lifecycle transition expected {:?} -> {:?} but saw {:?}",
                from,
                to,
                Lifecycle::from_u8(prev)
            );
        }
    }

    /// `Uninit → Sized`. Installs the boot-allocated frame descriptor
    /// table and zeroes the buddy's free-list counters.
    ///
    /// `frames_ptr` must point to `max_frames` properly aligned
    /// [`PageFrame`] slots with `'static` lifetime — the boot-time
    /// memory-init code allocates and HHDM-maps this region just
    /// before calling.
    pub(crate) fn install_descriptor_table(&self, frames_ptr: *mut u8, max_frames: u32) {
        if frames_ptr.is_null() || max_frames == 0 {
            panic!("BuddyAllocator::install_descriptor_table: null pointer or zero frames");
        }
        debug_assert_eq!(self.lifecycle(), Lifecycle::Uninit);

        let typed_ptr = frames_ptr as *mut PageFrame;
        let slice: &'static mut [PageFrame] =
            slopos_ostd::util::ptr_buf::borrow_buf_mut(typed_ptr, max_frames as usize);
        self.frame_table.install(slice);

        let mut inner = self.inner.lock();
        inner.total_frames = max_frames;
        inner.max_supported_frames = max_frames;
        inner.free_frames = 0;
        inner.allocated_frames = 0;
        inner.max_order = BuddyInner::derive_max_order(max_frames);
        inner.free_lists.fill(INVALID_PAGE_FRAME);

        for i in 0..max_frames {
            if let Some(frame) = inner.frame_desc_mut(&self.frame_table, i) {
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
            inner.max_order
        );

        drop(inner);
        self.advance(Lifecycle::Uninit, Lifecycle::Sized);
    }

    /// `Sized → Seeded`. Iterates the `mm_region_*` store and seeds
    /// the buddy free-lists from every usable region (respecting
    /// reservation flags).
    pub(crate) fn seed_from_memory_map(&self) {
        debug_assert_eq!(self.lifecycle(), Lifecycle::Sized);

        let mut inner = self.inner.lock();
        inner.free_lists.fill(INVALID_PAGE_FRAME);
        inner.free_frames = 0;
        inner.allocated_frames = 0;

        let region_count = mm_region_count();
        for i in 0..region_count {
            if let Some(region) = mm_region_get(i) {
                inner.seed_region_from_map(&self.frame_table, &region, i as u16);
            }
        }

        let free = inner.free_frames;
        drop(inner);
        self.advance(Lifecycle::Sized, Lifecycle::Seeded);
        klog_info!("Page allocator ready: {} pages available", free);
    }

    /// `Seeded → Live`. Activates the per-CPU page caches; subsequent
    /// order-0 allocations consult the PCP fast path.
    pub(crate) fn enable_pcp(&self) {
        debug_assert_eq!(self.lifecycle(), Lifecycle::Seeded);
        pcp::mark_live();
        self.advance(Lifecycle::Seeded, Lifecycle::Live);
        klog_info!("Per-CPU page cache enabled");
    }

    /// Lock-free descriptor reborrow for PCP fast-path use. Caller
    /// must hold a [`PreemptGuard`] **and** logical exclusivity over
    /// `frame_num` (e.g. the frame is in this CPU's PCP stack and
    /// the caller is about to pop it).
    #[inline]
    pub(super) fn frame_desc_lockfree<R>(
        &self,
        frame_num: u32,
        f: impl FnOnce(&mut PageFrame) -> R,
    ) -> Option<R> {
        self.frame_table.with_mut(frame_num as usize, f)
    }

    /// Acquire the buddy lock and run `f` with both the locked inner
    /// state and the (unlocked-but-exclusive-by-virtue-of-the-lock)
    /// frame descriptor table.
    #[inline]
    fn with_locked<R>(&self, f: impl FnOnce(&mut BuddyInner, &RawTable<PageFrame>) -> R) -> R {
        let mut inner = self.inner.lock();
        f(&mut inner, &self.frame_table)
    }

    /// Snapshot `(total, free, allocated)`. `free` folds in the PCP
    /// cached frames; `allocated` subtracts them so the two values
    /// continue to sum to a stable total.
    pub fn stats(&self) -> (u32, u32, u32) {
        let pcp_cached = pcp::total_cached();
        let inner = self.inner.lock();
        (
            inner.total_frames,
            inner.free_frames.saturating_add(pcp_cached),
            inner.allocated_frames.saturating_sub(pcp_cached),
        )
    }

    pub fn max_supported_frames(&self) -> u32 {
        self.inner.lock().max_supported_frames
    }

    pub fn pcp_stats(&self, cpu: usize) -> Option<(u32, u32, u32)> {
        pcp::snapshot(cpu)
    }

    pub fn frame_is_tracked(&self, phys_addr: PhysAddr) -> bool {
        let inner = self.inner.lock();
        let frame_num = inner.phys_to_frame(phys_addr);
        inner.is_valid_frame(frame_num)
    }

    pub fn frame_can_free(&self, phys_addr: PhysAddr) -> bool {
        let inner = self.inner.lock();
        let frame_num = inner.phys_to_frame(phys_addr);
        if !inner.is_valid_frame(frame_num) {
            return false;
        }
        let Some(frame) = inner.frame_desc_mut(&self.frame_table, frame_num) else {
            return false;
        };
        BuddyInner::frame_state_is_allocated(frame.state)
    }

    pub fn frame_inc_ref(&self, phys_addr: PhysAddr) -> Option<u32> {
        let inner = self.inner.lock();
        let frame_num = inner.phys_to_frame(phys_addr);
        if !inner.is_valid_frame(frame_num) {
            return None;
        }
        let frame = inner.frame_desc_mut(&self.frame_table, frame_num)?;
        if !BuddyInner::frame_state_is_allocated(frame.state) {
            return None;
        }
        frame.ref_count = frame.ref_count.saturating_add(1);
        Some(frame.ref_count)
    }

    pub fn frame_get_ref(&self, phys_addr: PhysAddr) -> u32 {
        let inner = self.inner.lock();
        let frame_num = inner.phys_to_frame(phys_addr);
        if !inner.is_valid_frame(frame_num) {
            return 0;
        }
        match inner.frame_desc_mut(&self.frame_table, frame_num) {
            Some(frame) => frame.ref_count,
            None => 0,
        }
    }

    /// Paint every tracked frame's contents with `value`. Used by
    /// soft-reboot scrub paths to wipe physical memory.
    pub fn paint_all(&self, value: u8) {
        let inner = self.inner.lock();
        if !self.frame_table.is_installed() {
            return;
        }
        for frame_num in 0..inner.total_frames {
            let phys_addr = inner.frame_to_phys(frame_num);
            if let Some(virt_addr) = phys_addr.to_virt_checked() {
                paint_page_at_virt(virt_addr.as_mut_ptr::<u8>(), value);
            }
        }
    }

    /// Drain every CPU's PCP back into the buddy. Shutdown only.
    pub fn drain_pcp_all(&self) {
        pcp::for_each_at_shutdown(|_cpu, cache| {
            let mut batch = [INVALID_PAGE_FRAME; pcp::PCP_BATCH_SIZE as usize];
            loop {
                if cache.count == 0 {
                    break;
                }
                let drained = self.with_locked(|inner, table| {
                    let mut drained = 0usize;
                    while drained < pcp::PCP_BATCH_SIZE as usize && cache.count > 0 {
                        cache.count -= 1;
                        let frame_num = cache.stack[cache.count as usize];
                        cache.stack[cache.count as usize] = INVALID_PAGE_FRAME;
                        batch[drained] = frame_num;
                        drained += 1;
                    }
                    if drained > 0 {
                        inner.free_batch_from_pcp(table, &batch[..drained]);
                    }
                    drained
                });
                if drained == 0 {
                    break;
                }
            }
        });
    }

    /// Raw multi-page buddy allocator entry point — bootstrap escape
    /// and policy-flag opt-out. The result is always zero-scrubbed.
    pub fn alloc_raw(&self, count: u32, flags: u32) -> PhysAddr {
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
            && pcp::is_live();

        let mut attempts = 0u32;
        loop {
            let frame_num = if use_pcp {
                let _no_migrate = PreemptGuard::new();
                let cpu = slopos_arch::pcr::get_current_cpu();
                let mut frame = self.pcp_try_alloc(cpu);
                if frame == INVALID_PAGE_FRAME {
                    self.pcp_refill(cpu, flags);
                    frame = self.pcp_try_alloc(cpu);
                }
                if frame == INVALID_PAGE_FRAME {
                    frame = self.with_locked(|inner, table| {
                        let flag_order = inner.flags_to_order(flags);
                        let actual_order = flag_order.max(order);
                        inner.allocate_block(table, actual_order, flags)
                    });
                }
                frame
            } else {
                self.with_locked(|inner, table| {
                    let flag_order = inner.flags_to_order(flags);
                    if flag_order > order {
                        order = flag_order;
                    }
                    inner.allocate_block(table, order, flags)
                })
            };

            if frame_num == INVALID_PAGE_FRAME {
                klog_info!("BuddyAllocator::alloc_raw: no suitable block available");
                return PhysAddr::NULL;
            }

            let phys_addr = self.with_locked(|inner, _table| inner.frame_to_phys(frame_num));

            let span_pages = if use_pcp {
                1
            } else {
                BuddyInner::order_block_pages(order)
            };
            let mut ok = true;
            for i in 0..span_pages {
                let page_phys = phys_addr.offset(i as u64 * PAGE_SIZE_4KB);
                if zero_physical_page(page_phys) != 0 {
                    klog_info!(
                        "BuddyAllocator::alloc_raw: failed to zero page at phys 0x{:x}",
                        page_phys.as_u64()
                    );
                    ok = false;
                    break;
                }
            }
            if !ok {
                attempts += 1;
                if attempts > 64 {
                    return PhysAddr::NULL;
                }
                continue;
            }

            return phys_addr;
        }
    }

    /// Free a single allocation back to the buddy. The buddy recovers
    /// the order from the descriptor; `size_pages` from
    /// [`FrameAlloc::dealloc`] is therefore ignored.
    pub fn free_phys(&self, phys_addr: PhysAddr) -> i32 {
        let _no_migrate = PreemptGuard::new();
        let cpu = slopos_arch::pcr::get_current_cpu();

        let mut inner = self.inner.lock();
        let frame_num = inner.phys_to_frame(phys_addr);
        if !inner.is_valid_frame(frame_num) {
            return -1;
        }

        let Some(frame) = inner.frame_desc_mut(&self.frame_table, frame_num) else {
            return -1;
        };
        if !BuddyInner::frame_state_is_allocated(frame.state) {
            return 0;
        }
        if frame.state == PAGE_FRAME_PCP {
            return 0;
        }
        if frame.ref_count > 1 {
            frame.ref_count -= 1;
            return 0;
        }

        let order = frame.order as u32;
        let is_pcp_candidate = order == 0 && frame.state == PAGE_FRAME_ALLOCATED && pcp::is_live();

        if is_pcp_candidate {
            if let Some(cache) = pcp::cache_mut(cpu) {
                if cache.count < pcp::PCP_HIGH_WATERMARK {
                    if let Some(desc) = inner.frame_desc_mut(&self.frame_table, frame_num) {
                        desc.state = PAGE_FRAME_PCP;
                        desc.ref_count = 0;
                        desc.next_free = INVALID_PAGE_FRAME;
                    }
                    cache.stack[cache.count as usize] = frame_num;
                    cache.count += 1;
                    cache
                        .free_count
                        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);

                    if cache.count > pcp::PCP_HIGH_WATERMARK {
                        let to_drain = (cache.count - pcp::PCP_HIGH_WATERMARK / 2)
                            .min(pcp::PCP_BATCH_SIZE)
                            as usize;
                        let mut batch = [INVALID_PAGE_FRAME; pcp::PCP_BATCH_SIZE as usize];
                        let mut drained = 0usize;
                        while drained < to_drain && cache.count > 0 {
                            cache.count -= 1;
                            batch[drained] = cache.stack[cache.count as usize];
                            cache.stack[cache.count as usize] = INVALID_PAGE_FRAME;
                            drained += 1;
                        }
                        if drained > 0 {
                            inner.free_batch_from_pcp(&self.frame_table, &batch[..drained]);
                        }
                    }
                    return 0;
                }
            }
        }

        if let Some(frame) = inner.frame_desc_mut(&self.frame_table, frame_num) {
            let pages = BuddyInner::order_block_pages(order);
            frame.ref_count = 0;
            frame.flags = 0;
            frame.state = PAGE_FRAME_FREE;
            inner.allocated_frames = inner.allocated_frames.saturating_sub(pages);
            inner.insert_block_coalescing(&self.frame_table, frame_num, order);
        }
        0
    }

    pub fn quarantine_allocated_phys(&self, phys_addr: PhysAddr) {
        if phys_addr.is_null() {
            return;
        }
        let mut inner = self.inner.lock();
        let frame_num = inner.phys_to_frame(phys_addr);
        if !inner.is_valid_frame(frame_num) {
            return;
        }
        let Some(frame) = inner.frame_desc_mut(&self.frame_table, frame_num) else {
            return;
        };
        if !BuddyInner::frame_state_is_allocated(frame.state) {
            return;
        }
        let pages = BuddyInner::order_block_pages(frame.order as u32);
        frame.ref_count = 0;
        frame.flags = 0;
        frame.next_free = INVALID_PAGE_FRAME;
        frame.state = PAGE_FRAME_NEVER_REUSE;
        inner.allocated_frames = inner.allocated_frames.saturating_sub(pages);
    }

    /// Batch-allocate up to `out.len()` zeroed order-0 pages under a
    /// single [`PreemptGuard`]. Returns the number of slots filled.
    pub fn alloc_pcp_batch(&self, out: &mut [PhysAddr]) -> usize {
        if out.is_empty() {
            return 0;
        }
        if out.len() > pcp::PCP_CAPACITY {
            let mut filled = 0usize;
            for slot in out.iter_mut() {
                let pa = self.alloc_raw(1, 0);
                if pa.is_null() {
                    break;
                }
                *slot = pa;
                filled += 1;
            }
            return filled;
        }

        let mut frames = [INVALID_PAGE_FRAME; pcp::PCP_CAPACITY];
        let mut filled = 0usize;

        if pcp::is_live() {
            let _no_migrate = PreemptGuard::new();
            let cpu = slopos_arch::pcr::get_current_cpu();
            while filled < out.len() {
                let mut frame = self.pcp_try_alloc(cpu);
                if frame == INVALID_PAGE_FRAME {
                    self.pcp_refill(cpu, 0);
                    frame = self.pcp_try_alloc(cpu);
                    if frame == INVALID_PAGE_FRAME {
                        break;
                    }
                }
                frames[filled] = frame;
                filled += 1;
            }
        }

        while filled < out.len() {
            let frame_num = self.with_locked(|inner, table| inner.allocate_block(table, 0, 0));
            if frame_num == INVALID_PAGE_FRAME {
                break;
            }
            frames[filled] = frame_num;
            filled += 1;
        }

        if filled > 0 {
            self.with_locked(|inner, _table| {
                for i in 0..filled {
                    out[i] = inner.frame_to_phys(frames[i]);
                }
            });
            for i in 0..filled {
                if zero_physical_page(out[i]) != 0 {
                    klog_info!(
                        "alloc_pcp_batch: zero_physical_page failed at 0x{:x}",
                        out[i].as_u64()
                    );
                }
            }
        }
        filled
    }

    // -----------------------------------------------------------------------
    // PCP fast-path helpers. Live on `BuddyAllocator` (not `pcp`) so they
    // can use both the lock-free descriptor table and the locked refill
    // path through a single object.
    // -----------------------------------------------------------------------

    fn pcp_try_alloc(&self, cpu: usize) -> u32 {
        debug_assert!(PreemptGuard::is_active());
        let Some(cache) = pcp::cache_mut(cpu) else {
            return INVALID_PAGE_FRAME;
        };
        if !pcp::is_live() || cache.count == 0 {
            return INVALID_PAGE_FRAME;
        }
        cache.count -= 1;
        let frame_num = cache.stack[cache.count as usize];
        cache.stack[cache.count as usize] = INVALID_PAGE_FRAME;
        cache
            .alloc_count
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.frame_desc_lockfree(frame_num, |desc| {
            desc.state = PAGE_FRAME_ALLOCATED;
            desc.ref_count = 1;
            desc.next_free = INVALID_PAGE_FRAME;
        });
        frame_num
    }

    fn pcp_refill(&self, cpu: usize, flags: u32) {
        debug_assert!(PreemptGuard::is_active());
        let Some(cache) = pcp::cache_mut(cpu) else {
            return;
        };
        if cache.count >= pcp::PCP_LOW_WATERMARK {
            return;
        }

        let mut batch = [INVALID_PAGE_FRAME; pcp::PCP_BATCH_SIZE as usize];
        let needed = pcp::PCP_BATCH_SIZE.min(pcp::PCP_HIGH_WATERMARK - cache.count);

        let allocated = self.with_locked(|inner, table| {
            if cache.count >= pcp::PCP_HIGH_WATERMARK {
                return 0;
            }
            let count = inner.allocate_batch_for_pcp(table, &mut batch[..needed as usize], flags);
            for i in 0..count {
                let frame_num = batch[i];
                if let Some(desc) = inner.frame_desc_mut(table, frame_num) {
                    desc.state = PAGE_FRAME_PCP;
                    desc.ref_count = 0;
                    desc.next_free = INVALID_PAGE_FRAME;
                }
            }
            count
        });

        for i in 0..allocated {
            if (cache.count as usize) >= pcp::PCP_CAPACITY {
                break;
            }
            cache.stack[cache.count as usize] = batch[i];
            cache.count += 1;
        }
    }
}

impl FrameAlloc for BuddyAllocator {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr> {
        debug_assert_eq!(
            opts.align_pages, 1,
            "BuddyAllocator only supports align_pages == 1"
        );
        // The buddy unconditionally scrubs; `opts.zeroing` is a
        // type-level audit signal handled by the OSTD typestate, not
        // a runtime perf escape.
        let count = u32::try_from(opts.size_pages.max(1)).ok()?;
        let mut flags = 0u32;
        if opts.no_pcp {
            flags |= ALLOC_FLAG_NO_PCP;
        }
        if opts.dma {
            flags |= ALLOC_FLAG_DMA;
        }
        let phys = self.alloc_raw(count, flags);
        // LUF reuse-drain hook: if the frame is still referenced by a
        // deferred TLB flush on this CPU, drain the queue before the
        // new owner installs a mapping. Single-page allocations only,
        // matching the pre-refactor behaviour (multi-page callers do
        // their own TLB management).
        if !phys.is_null() && count == 1 {
            if !crate::mmu::luf::drain_if_reusing_frame(phys) {
                self.quarantine_allocated_phys(phys);
                return None;
            }
        }
        if phys.is_null() { None } else { Some(phys) }
    }

    fn dealloc(&self, paddr: Paddr, _size_pages: usize) {
        let _ = self.free_phys(paddr);
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

#[inline]
fn paint_page_at_virt(ptr: *mut u8, value: u8) {
    if ptr.is_null() {
        return;
    }
    slopos_ostd::util::ptr_buf::borrow_buf_mut(ptr, PAGE_SIZE_4KB as usize).fill(value);
}

fn zero_physical_page(phys_addr: PhysAddr) -> i32 {
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
