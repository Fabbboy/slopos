//! Buddy allocator core for the kernel's physical page frames.
//!
//! [`BuddyAllocator`] is the safe-Rust [`FrameAlloc`] implementation OSTD
//! registers; the single static instance lives in [`super`].
//!
//! Access to the [`RawTable<PageFrame>`] descriptor table is gated either by
//! the [`SpinLock`] over [`BuddyInner`] (when mutating free-list links) or by
//! exclusive PCP ownership under a [`PreemptGuard`].

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use slopos_ostd::lock_class;

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::frame::{FrameAlloc, FrameAllocOptions, Paddr};
use slopos_ostd::panic::AbortOnUnwind;
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_ALLOCATOR, PreemptGuard, RawTable, SpinLock};
use slopos_ostd::util::ptr_buf::OneShotBuf;
use slopos_ostd::{align_down_u64, align_up_u64, klog_debug, klog_info};

use crate::hhdm::PhysAddrHhdm;
use crate::memory_reservations::{
    MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS, MmRegion, MmRegionKind, mm_region_count, mm_region_get,
    mm_reservations_count, mm_reservations_get,
};
use crate::paging_defs::PAGE_SIZE_4KB;

use super::pcp;

pub const ALLOC_FLAG_DMA: u32 = 0x02;
pub const ALLOC_FLAG_KERNEL: u32 = 0x04;
pub const ALLOC_FLAG_ORDER_SHIFT: u32 = 8;
pub const ALLOC_FLAG_ORDER_MASK: u32 = 0x1F << ALLOC_FLAG_ORDER_SHIFT;
pub const ALLOC_FLAG_NO_PCP: u32 = 0x80;

pub(super) const PAGE_FRAME_FREE: u8 = 0x00;
pub(super) const PAGE_FRAME_ALLOCATED: u8 = 0x01;
pub(super) const PAGE_FRAME_RESERVED: u8 = 0x02;
pub(super) const PAGE_FRAME_KERNEL: u8 = 0x03;
pub(super) const PAGE_FRAME_DMA: u8 = 0x04;
pub(super) const PAGE_FRAME_PCP: u8 = 0x05;
pub(super) const PAGE_FRAME_NEVER_REUSE: u8 = 0x06;
/// Freed, but parked until every CPU has invalidated. See [`crate::mmu::quiesce`].
pub(super) const PAGE_FRAME_QUIESCE: u8 = 0x07;

pub(super) const INVALID_PAGE_FRAME: u32 = 0xFFFF_FFFF;
pub(super) const MAX_ORDER: u32 = 24;

/// One-shot guard for the frame-descriptor table's `&'static mut` handover.
static FRAME_TABLE_CLAIMED: InitFlag = InitFlag::new();

const INVALID_REGION_ID: u16 = 0xFFFF;
const DMA_MEMORY_LIMIT: u64 = 0x0100_0000;

/// Closing an epoch costs one all-context invalidation per CPU, so amortise it
/// over a batch rather than paying per free. 1024 frames is 4 MiB held.
const QUARANTINE_ADVANCE_FRAMES: u32 = 1024;

/// Coalescing scans a free list to find the buddy, so bound how long one
/// release pass holds the allocator's cli-lock.
const QUARANTINE_RELEASE_BATCH: u32 = 64;

/// Release budget charged against every quarantining free, in pages not blocks.
const QUARANTINE_RELEASE_PER_FREE: u32 = 8;

/// Written under the buddy lock, read from the timer tick — which must not take
/// the allocator's lock 100 times a second to ask a yes/no question.
pub(super) static QUARANTINE_FRAMES: AtomicU32 = AtomicU32::new(0);

#[cfg(feature = "test-hooks")]
static ROTATE_SPLICED_PAGES: AtomicU32 = AtomicU32::new(0);

/// Where the buddy accounts one frame.
#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameAccounting {
    Untracked,
    HandedOut,
    Cached,
    Quarantined,
    Free,
    Withheld,
}

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

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct PageFrame {
    pub(super) state: u8,
    pub(super) flags: u8,
    pub(super) order: u16,
    pub(super) region_id: u16,
    pub(super) next_free: u32,
}

#[derive(Default)]
pub(super) struct BuddyInner {
    pub(super) total_frames: u32,
    pub(super) max_supported_frames: u32,
    pub(super) free_frames: u32,
    pub(super) allocated_frames: u32,
    pub(super) free_lists: [u32; (MAX_ORDER as usize) + 1],
    pub(super) max_order: u32,

    /// Blocks freed during the open epoch, chained through `next_free`:
    /// unlinked from their owner but not yet eligible to be handed out.
    pub(super) quarantine_incoming: u32,
    pub(super) quarantine_incoming_tail: u32,
    /// Blocks freed during the previous epoch. These become eligible the
    /// moment the open epoch closes; see [`crate::mmu::quiesce`] for why it is
    /// the *second* closure and not the first that proves them safe.
    pub(super) quarantine_draining: u32,
    pub(super) quarantine_draining_tail: u32,
    /// Blocks the epoch has already proven safe, waiting to be spliced back
    /// into the free lists.
    pub(super) quarantine_releasable: u32,
    pub(super) quarantine_releasable_tail: u32,
    /// Frames held across all three lists.
    pub(super) quarantine_frames: u32,
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
            quarantine_incoming: INVALID_PAGE_FRAME,
            quarantine_incoming_tail: INVALID_PAGE_FRAME,
            quarantine_draining: INVALID_PAGE_FRAME,
            quarantine_draining_tail: INVALID_PAGE_FRAME,
            quarantine_releasable: INVALID_PAGE_FRAME,
            quarantine_releasable_tail: INVALID_PAGE_FRAME,
            quarantine_frames: 0,
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

    /// Park a just-freed block without coalescing: merging with a free buddy
    /// would splice a not-yet-reusable frame into a handout-eligible block.
    fn quarantine_push(&mut self, table: &RawTable<PageFrame>, frame_num: u32, order: u32) {
        let head = self.quarantine_incoming;
        if let Some(frame) = self.frame_desc_mut(table, frame_num) {
            frame.next_free = head;
            frame.order = order as u16;
            frame.state = PAGE_FRAME_QUIESCE;
            frame.flags = 0;
            self.quarantine_incoming = frame_num;
            if self.quarantine_incoming_tail == INVALID_PAGE_FRAME {
                self.quarantine_incoming_tail = frame_num;
            }
            self.quarantine_frames = self
                .quarantine_frames
                .saturating_add(Self::order_block_pages(order));
            QUARANTINE_FRAMES.store(self.quarantine_frames, Ordering::Relaxed);
        }
    }

    /// O(1) concat — the entire point of tracking tails.
    fn quarantine_concat(
        table: &RawTable<PageFrame>,
        src: (u32, u32),
        dest: (u32, u32),
    ) -> (u32, u32) {
        let (src_head, src_tail) = src;
        let (dest_head, dest_tail) = dest;
        if src_head == INVALID_PAGE_FRAME {
            return dest;
        }
        if dest_head == INVALID_PAGE_FRAME {
            return src;
        }
        if let Some(tail) = table.get_mut(src_tail as usize) {
            tail.next_free = dest_head;
        }
        (src_head, dest_tail)
    }

    /// Close one epoch: `draining` joins the releasable backlog, `incoming`
    /// takes its place.
    ///
    /// Splices nothing — a splice is O(blocks × free-list length), and this
    /// runs from whichever CPU's timer interrupt observes the last ack.
    fn quarantine_rotate(&mut self, table: &RawTable<PageFrame>) -> u32 {
        let free_before = self.free_frames;
        let releasable = Self::quarantine_concat(
            table,
            (self.quarantine_draining, self.quarantine_draining_tail),
            (self.quarantine_releasable, self.quarantine_releasable_tail),
        );
        self.quarantine_releasable = releasable.0;
        self.quarantine_releasable_tail = releasable.1;

        self.quarantine_draining = self.quarantine_incoming;
        self.quarantine_draining_tail = self.quarantine_incoming_tail;
        self.quarantine_incoming = INVALID_PAGE_FRAME;
        self.quarantine_incoming_tail = INVALID_PAGE_FRAME;

        let grew = self.free_frames.saturating_sub(free_before);
        #[cfg(feature = "test-hooks")]
        ROTATE_SPLICED_PAGES.fetch_add(grew, Ordering::Relaxed);
        grew
    }

    /// Splice up to `limit` pages from the releasable backlog into the free
    /// lists.
    fn quarantine_release_some(&mut self, table: &RawTable<PageFrame>, limit: u32) -> u32 {
        let mut released = 0u32;
        while released < limit {
            let cursor = self.quarantine_releasable;
            if cursor == INVALID_PAGE_FRAME {
                self.quarantine_releasable_tail = INVALID_PAGE_FRAME;
                break;
            }
            let Some(frame) = self.frame_desc_mut(table, cursor) else {
                self.quarantine_releasable = INVALID_PAGE_FRAME;
                self.quarantine_releasable_tail = INVALID_PAGE_FRAME;
                break;
            };
            let next = frame.next_free;
            let order = frame.order as u32;
            // Mark free before coalescing: a block still labelled QUIESCE must
            // not be merged into.
            frame.state = PAGE_FRAME_FREE;
            frame.next_free = INVALID_PAGE_FRAME;
            self.quarantine_releasable = next;
            if next == INVALID_PAGE_FRAME {
                self.quarantine_releasable_tail = INVALID_PAGE_FRAME;
            }
            self.insert_block_coalescing(table, cursor, order);
            released = released.saturating_add(Self::order_block_pages(order));
        }
        if released > 0 {
            self.quarantine_frames = self.quarantine_frames.saturating_sub(released);
            QUARANTINE_FRAMES.store(self.quarantine_frames, Ordering::Relaxed);
        }
        released
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

            // The buddy's pages are already in `free_frames`; the single push
            // below re-adds them as part of the merged block.
            self.free_frames = self
                .free_frames
                .saturating_sub(Self::order_block_pages(curr_order));

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

pub struct BuddyAllocator {
    inner: SpinLock<BuddyInner>,
    frame_table: RawTable<PageFrame>,
    state: AtomicU8,
}

impl BuddyAllocator {
    /// Const constructor for the BSS-resident singleton in
    /// [`super::BUDDY_ALLOCATOR`]. Caller must drive
    /// `Uninit → Sized → Seeded → Live` before OSTD reads from it.
    pub const fn new_uninit() -> Self {
        Self {
            inner: SpinLock::new(
                BuddyInner::new(),
                lock_class!("BUDDY_ALLOCATOR", LOCK_LEVEL_ALLOCATOR),
            ),
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

    /// `Uninit → Sized`. Installs the boot-allocated frame descriptor table.
    ///
    /// `frames_ptr` must point to `max_frames` properly aligned
    /// [`PageFrame`] slots with `'static` lifetime.
    pub(crate) fn install_descriptor_table(&self, frames_ptr: *mut u8, max_frames: u32) {
        let Some(typed_ptr) =
            NonNull::new(frames_ptr.cast::<PageFrame>()).filter(|_| max_frames > 0)
        else {
            panic!("BuddyAllocator::install_descriptor_table: null pointer or zero frames");
        };
        debug_assert_eq!(self.lifecycle(), Lifecycle::Uninit);

        // `'static` holds because the region is boot-reserved for the life of
        // the machine; the claim makes a second install fail rather than mint
        // a second `&'static mut` to the same descriptors.
        let claim = OneShotBuf::claim(&FRAME_TABLE_CLAIMED, typed_ptr, max_frames as usize)
            .expect("BuddyAllocator::install_descriptor_table called twice");
        self.frame_table.install(claim.into_static_mut());

        let mut inner = self.inner.lock();
        inner.total_frames = max_frames;
        inner.max_supported_frames = max_frames;
        inner.free_frames = 0;
        inner.allocated_frames = 0;
        inner.max_order = BuddyInner::derive_max_order(max_frames);
        inner.free_lists.fill(INVALID_PAGE_FRAME);

        for i in 0..max_frames {
            if let Some(frame) = inner.frame_desc_mut(&self.frame_table, i) {
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

    /// `Sized → Seeded`. Seeds the free lists from every usable `mm_region_*`
    /// entry, respecting reservation flags.
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

    /// `Seeded → Live`. Activates the per-CPU page caches.
    pub(crate) fn enable_pcp(&self) {
        debug_assert_eq!(self.lifecycle(), Lifecycle::Seeded);
        pcp::mark_live();
        self.advance(Lifecycle::Seeded, Lifecycle::Live);
        klog_info!("Per-CPU page cache enabled");
    }

    /// Lock-free descriptor reborrow. Caller must hold a [`PreemptGuard`]
    /// **and** logical exclusivity over `frame_num` (e.g. the frame is in this
    /// CPU's PCP stack and the caller is about to pop it).
    #[inline]
    pub(super) fn frame_desc_lockfree<R>(
        &self,
        frame_num: u32,
        f: impl FnOnce(&mut PageFrame) -> R,
    ) -> Option<R> {
        self.frame_table.with_mut(frame_num as usize, f)
    }

    /// Run `f` under the buddy lock, with the frame descriptor table held
    /// exclusive by virtue of that lock.
    ///
    /// Free-list mutation is multi-step, so an unwind mid-mutation would
    /// release the lock around a torn free list; it aborts instead.
    #[inline]
    fn with_locked<R>(&self, f: impl FnOnce(&mut BuddyInner, &RawTable<PageFrame>) -> R) -> R {
        let mut inner = self.inner.lock();
        let abort_guard = AbortOnUnwind::new();
        let result = f(&mut inner, &self.frame_table);
        abort_guard.disarm();
        result
    }

    /// Snapshot `(total, free, allocated)`. `free` folds in the PCP cached
    /// frames and `allocated` subtracts them, so the two still sum to a
    /// stable total.
    /// Cached frames are counted under the lock, or a peer's drain double-counts.
    pub fn stats(&self) -> (u32, u32, u32) {
        let inner = self.inner.lock();
        let pcp_cached = pcp::total_cached();
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

    #[cfg(feature = "test-hooks")]
    pub fn rotate_spliced_pages(&self) -> u32 {
        ROTATE_SPLICED_PAGES.load(Ordering::Relaxed)
    }

    #[cfg(feature = "test-hooks")]
    pub fn frame_accounting(&self, phys_addr: PhysAddr) -> FrameAccounting {
        self.with_locked(|inner, table| {
            let frame_num = inner.phys_to_frame(phys_addr);
            match inner.frame_desc_mut(table, frame_num).map(|f| f.state) {
                None => FrameAccounting::Untracked,
                Some(PAGE_FRAME_ALLOCATED | PAGE_FRAME_KERNEL | PAGE_FRAME_DMA) => {
                    FrameAccounting::HandedOut
                }
                Some(PAGE_FRAME_PCP) => FrameAccounting::Cached,
                Some(PAGE_FRAME_QUIESCE) => FrameAccounting::Quarantined,
                Some(PAGE_FRAME_FREE) => FrameAccounting::Free,
                Some(_) => FrameAccounting::Withheld,
            }
        })
    }

    pub fn frame_is_tracked(&self, phys_addr: PhysAddr) -> bool {
        let inner = self.inner.lock();
        let frame_num = inner.phys_to_frame(phys_addr);
        inner.is_valid_frame(frame_num)
    }

    /// Paint every tracked frame's contents with `value`.
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

    /// Raw multi-page entry point — bootstrap escape and policy-flag opt-out.
    /// The result is always zero-scrubbed.
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
        let mut quiesce_recovered = false;
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
                // Memory may just be parked awaiting a quiesce, and a
                // quarantined block is uncoalesced, so a backlog fails a
                // multi-page request long before a single-page one. Drain the
                // backlog, then ack — which closes the epoch outright if the
                // peers already have. Never waits on a peer.
                if !quiesce_recovered && crate::mmu::quiesce::is_active() {
                    quiesce_recovered = true;
                    let mut released = self.quarantine_drain_backlog();
                    crate::mmu::quiesce::ack_now();
                    released += self.quarantine_drain_backlog();
                    if released > 0 {
                        continue;
                    }
                }
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

    /// Free a single allocation back to the buddy. The order is recovered from
    /// the descriptor, so [`FrameAlloc::dealloc`]'s `size_pages` is ignored.
    pub fn free_phys(&self, phys_addr: PhysAddr) -> i32 {
        let _no_migrate = PreemptGuard::new();
        let cpu = slopos_arch::pcr::get_current_cpu();

        self.with_locked(|inner, table| {
            let frame_num = inner.phys_to_frame(phys_addr);
            if !inner.is_valid_frame(frame_num) {
                return -1;
            }

            let Some(frame) = inner.frame_desc_mut(table, frame_num) else {
                return -1;
            };
            if !BuddyInner::frame_state_is_allocated(frame.state) {
                return 0;
            }
            if frame.state == PAGE_FRAME_PCP {
                return 0;
            }

            let order = frame.order as u32;

            // Ahead of the PCP magazine as well as the free lists: the magazine
            // is a reuse path too, and a frame re-handed-out from it never
            // touches the buddy at all.
            if crate::mmu::quiesce::quarantine_required() {
                let pages = BuddyInner::order_block_pages(order);
                inner.allocated_frames = inner.allocated_frames.saturating_sub(pages);
                inner.quarantine_push(table, frame_num, order);
                // Pay down the release debt on every free; otherwise a workload
                // that never idles parks memory until allocations fail.
                inner.quarantine_release_some(table, QUARANTINE_RELEASE_PER_FREE);
                if inner.quarantine_frames >= QUARANTINE_ADVANCE_FRAMES {
                    crate::mmu::quiesce::request_advance();
                }
                return 0;
            }

            let is_pcp_candidate =
                order == 0 && frame.state == PAGE_FRAME_ALLOCATED && pcp::is_live();

            if is_pcp_candidate {
                if let Some(cache) = pcp::cache_mut(cpu) {
                    if cache.count < pcp::PCP_HIGH_WATERMARK {
                        if let Some(desc) = inner.frame_desc_mut(table, frame_num) {
                            desc.state = PAGE_FRAME_PCP;
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
                                inner.free_batch_from_pcp(table, &batch[..drained]);
                            }
                        }
                        return 0;
                    }
                }
            }

            if let Some(frame) = inner.frame_desc_mut(table, frame_num) {
                let pages = BuddyInner::order_block_pages(order);
                frame.flags = 0;
                frame.state = PAGE_FRAME_FREE;
                inner.allocated_frames = inner.allocated_frames.saturating_sub(pages);
                inner.insert_block_coalescing(table, frame_num, order);
            }
            0
        })
    }

    /// Promote the proven-safe batch into the releasable backlog. O(1); the
    /// splicing is [`Self::quarantine_release_some`]'s job.
    pub fn quarantine_rotate(&self) -> u32 {
        self.with_locked(|inner, table| inner.quarantine_rotate(table))
    }

    /// Splice up to `limit` proven-safe blocks back into the free lists.
    /// Returns the number of frames released.
    pub fn quarantine_release_some(&self, limit: u32) -> u32 {
        self.with_locked(|inner, table| inner.quarantine_release_some(table, limit))
    }

    /// Drain the whole backlog in bounded steps.
    pub fn quarantine_drain_backlog(&self) -> u32 {
        let mut total = 0u32;
        loop {
            let released = self.quarantine_release_some(QUARANTINE_RELEASE_BATCH);
            if released == 0 {
                return total;
            }
            total = total.saturating_add(released);
        }
    }

    pub fn quarantine_has_releasable(&self) -> bool {
        self.with_locked(|inner, _table| inner.quarantine_releasable != INVALID_PAGE_FRAME)
    }

    pub fn quarantine_is_occupied(&self) -> bool {
        QUARANTINE_FRAMES.load(Ordering::Relaxed) > 0
    }

    pub fn quarantine_frames(&self) -> u32 {
        QUARANTINE_FRAMES.load(Ordering::Relaxed)
    }

    pub fn quarantine_allocated_phys(&self, phys_addr: PhysAddr) {
        if phys_addr.is_null() {
            return;
        }
        self.with_locked(|inner, table| {
            let frame_num = inner.phys_to_frame(phys_addr);
            if !inner.is_valid_frame(frame_num) {
                return;
            }
            let Some(frame) = inner.frame_desc_mut(table, frame_num) else {
                return;
            };
            if !BuddyInner::frame_state_is_allocated(frame.state) {
                return;
            }
            let pages = BuddyInner::order_block_pages(frame.order as u32);
            frame.flags = 0;
            frame.next_free = INVALID_PAGE_FRAME;
            frame.state = PAGE_FRAME_NEVER_REUSE;
            inner.allocated_frames = inner.allocated_frames.saturating_sub(pages);
        })
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

    // No unwind-abort guard on the PCP spans below: they are straight-line
    // field writes with no fallible calls, and the worst torn outcome is a
    // leaked frame, never a torn list.
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
        // The buddy unconditionally scrubs; `opts.zeroing` is a type-level
        // audit signal, not a runtime perf escape.
        let count = u32::try_from(opts.size_pages.max(1)).ok()?;
        let mut flags = 0u32;
        if opts.no_pcp {
            flags |= ALLOC_FLAG_NO_PCP;
        }
        if opts.dma {
            flags |= ALLOC_FLAG_DMA;
        }
        // No cross-CPU work here: `alloc` is reachable from page-fault handlers
        // and from under interrupt-disabling locks, so a rendezvous deadlocks
        // against its own callers. A frame reaches a free list only after
        // `mmu::quiesce` proved every CPU invalidated since it was unmapped.
        let phys = self.alloc_raw(count, flags);
        if phys.is_null() { None } else { Some(phys) }
    }

    fn dealloc(&self, paddr: Paddr, _size_pages: usize) {
        let _ = self.free_phys(paddr);
    }
}

#[inline]
fn paint_page_at_virt(ptr: *mut u8, value: u8) {
    if ptr.is_null() {
        return;
    }
    slopos_ostd::util::ptr_buf::with_buf_mut(ptr, PAGE_SIZE_4KB as usize, |page: &mut [u8]| {
        page.fill(value)
    });
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
