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
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_lib::{IrqMutex, align_down_u64, align_up_u64, klog_debug, klog_info};

use crate::hhdm::PhysAddrHhdm;
use crate::memory_reservations::{MmRegion, MmRegionKind, mm_region_count, mm_region_get};
use crate::mm_constants::PAGE_SIZE_4KB;

pub const ALLOC_FLAG_ZERO: u32 = 0x01;
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

const MAX_CPUS: usize = 256;
const PCP_HIGH_WATERMARK: u32 = 64;
const PCP_LOW_WATERMARK: u32 = 8;
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

#[repr(C, align(64))]
struct PerCpuPageCache {
    head: AtomicU32,
    count: AtomicU32,
    alloc_count: AtomicU32,
    free_count: AtomicU32,
    _pad: [u32; 12],
}

impl PerCpuPageCache {
    const fn new() -> Self {
        Self {
            head: AtomicU32::new(INVALID_PAGE_FRAME),
            count: AtomicU32::new(0),
            alloc_count: AtomicU32::new(0),
            free_count: AtomicU32::new(0),
            _pad: [0; 12],
        }
    }
}

static PER_CPU_CACHES: [PerCpuPageCache; MAX_CPUS] = {
    const INIT: PerCpuPageCache = PerCpuPageCache::new();
    [INIT; MAX_CPUS]
};

static PCP_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct PageAllocator {
    frames: *mut PageFrame,
    total_frames: u32,
    max_supported_frames: u32,
    free_frames: u32,
    allocated_frames: u32,
    free_lists: [u32; (MAX_ORDER as usize) + 1],
    max_order: u32,
}

unsafe impl Send for PageAllocator {}

impl PageAllocator {
    const fn new() -> Self {
        Self {
            frames: ptr::null_mut(),
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

    unsafe fn frame_desc_mut(&self, frame_num: u32) -> Option<&'static mut PageFrame> {
        if !self.is_valid_frame(frame_num) || self.frames.is_null() {
            return None;
        }
        Some(unsafe { &mut *self.frames.add(frame_num as usize) })
    }

    fn frame_region_id(&self, frame_num: u32) -> u16 {
        unsafe { self.frame_desc_mut(frame_num) }
            .map(|f| f.region_id)
            .unwrap_or(INVALID_REGION_ID)
    }

    fn order_block_pages(order: u32) -> u32 {
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
        if let Some(frame) = unsafe { self.frame_desc_mut(frame_num) } {
            frame.next_free = self.free_lists[order as usize];
            frame.order = order as u16;
            frame.state = PAGE_FRAME_FREE;
            frame.flags = 0;
            frame.ref_count = 0;
            self.free_lists[order as usize] = frame_num;
        }
    }

    fn free_list_detach(&mut self, order: u32, target_frame: u32) -> bool {
        let head_ptr = self.free_lists.as_mut_ptr().wrapping_add(order as usize);
        let mut prev = INVALID_PAGE_FRAME;
        let mut current = unsafe { *head_ptr };

        while current != INVALID_PAGE_FRAME {
            if current == target_frame {
                let next = unsafe { self.frame_desc_mut(current) }
                    .map(|f| f.next_free)
                    .unwrap_or(INVALID_PAGE_FRAME);
                if prev == INVALID_PAGE_FRAME {
                    unsafe { *head_ptr = next };
                } else if let Some(prev_desc) = unsafe { self.frame_desc_mut(prev) } {
                    prev_desc.next_free = next;
                }
                if let Some(curr_desc) = unsafe { self.frame_desc_mut(current) } {
                    curr_desc.next_free = INVALID_PAGE_FRAME;
                }
                return true;
            }
            prev = current;
            current = unsafe { self.frame_desc_mut(current) }
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
        let head_ptr = self.free_lists.as_mut_ptr().wrapping_add(order as usize);
        let mut prev = INVALID_PAGE_FRAME;
        let mut current = unsafe { *head_ptr };

        while current != INVALID_PAGE_FRAME {
            if self.block_meets_flags(current, order, flags) {
                let next = unsafe { self.frame_desc_mut(current) }
                    .map(|f| f.next_free)
                    .unwrap_or(INVALID_PAGE_FRAME);
                if prev == INVALID_PAGE_FRAME {
                    unsafe { *head_ptr = next };
                } else if let Some(prev_desc) = unsafe { self.frame_desc_mut(prev) } {
                    prev_desc.next_free = next;
                }
                if let Some(curr_desc) = unsafe { self.frame_desc_mut(current) } {
                    curr_desc.next_free = INVALID_PAGE_FRAME;
                }

                let pages = Self::order_block_pages(order);
                if self.free_frames >= pages {
                    self.free_frames -= pages;
                }
                return current;
            }

            prev = current;
            current = unsafe { self.frame_desc_mut(current) }
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
            let buddy_desc = unsafe { self.frame_desc_mut(buddy) };

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

            if let Some(desc) = unsafe { self.frame_desc_mut(block) } {
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
            if let Some(desc) = unsafe { self.frame_desc_mut(frame_num) } {
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
            if let Some(desc) = unsafe { self.frame_desc_mut(frame_num) } {
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

        let aligned_start = align_up_u64(region.phys_base, PAGE_SIZE_4KB);
        let aligned_end = align_down_u64(region.phys_base + region.length, PAGE_SIZE_4KB);
        if aligned_end <= aligned_start {
            return;
        }

        let start_frame = self.phys_to_frame(PhysAddr::new(aligned_start));
        let mut end_frame = self.phys_to_frame(PhysAddr::new(aligned_end));
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
                if let Some(f) = unsafe { self.frame_desc_mut(frame + i) } {
                    f.region_id = seeded_id;
                }
            }
            self.insert_block_coalescing(frame, order);
            frame += block_pages;
            remaining -= block_pages;
        }
    }
}

static PAGE_ALLOCATOR: IrqMutex<PageAllocator> = IrqMutex::new(PageAllocator::new());

const DMA_MEMORY_LIMIT: u64 = 0x0100_0000;

#[inline]
fn get_current_cpu() -> usize {
    0
}

fn pcp_try_alloc(cpu: usize) -> u32 {
    if cpu >= MAX_CPUS || !PCP_INITIALIZED.load(Ordering::Acquire) {
        return INVALID_PAGE_FRAME;
    }

    let cache = &PER_CPU_CACHES[cpu];

    // Try to pop from the cache's free list
    // Use a simple CAS loop for lock-free operation
    loop {
        let head = cache.head.load(Ordering::Acquire);
        if head == INVALID_PAGE_FRAME {
            return INVALID_PAGE_FRAME;
        }

        // Read the next pointer from the frame descriptor
        let next = {
            let alloc = PAGE_ALLOCATOR.lock();
            unsafe { alloc.frame_desc_mut(head) }
                .map(|f| f.next_free)
                .unwrap_or(INVALID_PAGE_FRAME)
        };

        if cache
            .head
            .compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            cache.count.fetch_sub(1, Ordering::Relaxed);
            cache.alloc_count.fetch_add(1, Ordering::Relaxed);

            {
                let alloc = PAGE_ALLOCATOR.lock();
                if let Some(desc) = unsafe { alloc.frame_desc_mut(head) } {
                    desc.state = PAGE_FRAME_ALLOCATED;
                    desc.ref_count = 1;
                    desc.next_free = INVALID_PAGE_FRAME;
                }
            }

            return head;
        }
    }
}

fn pcp_try_free(cpu: usize, frame_num: u32) -> bool {
    if cpu >= MAX_CPUS || !PCP_INITIALIZED.load(Ordering::Acquire) {
        return false;
    }

    let cache = &PER_CPU_CACHES[cpu];

    let current_count = cache.count.load(Ordering::Relaxed);
    if current_count >= PCP_HIGH_WATERMARK {
        return false;
    }

    loop {
        let head = cache.head.load(Ordering::Acquire);

        {
            let alloc = PAGE_ALLOCATOR.lock();
            if let Some(desc) = unsafe { alloc.frame_desc_mut(frame_num) } {
                desc.next_free = head;
                desc.state = PAGE_FRAME_PCP;
                desc.ref_count = 0;
            }
        }

        if cache
            .head
            .compare_exchange_weak(head, frame_num, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            cache.count.fetch_add(1, Ordering::Relaxed);
            cache.free_count.fetch_add(1, Ordering::Relaxed);
            return true;
        }
    }
}

fn pcp_refill(cpu: usize, flags: u32) {
    if cpu >= MAX_CPUS {
        return;
    }

    let cache = &PER_CPU_CACHES[cpu];
    let current_count = cache.count.load(Ordering::Relaxed);

    if current_count >= PCP_LOW_WATERMARK {
        return;
    }

    let needed = PCP_BATCH_SIZE.min(PCP_HIGH_WATERMARK - current_count);
    let mut batch = [INVALID_PAGE_FRAME; PCP_BATCH_SIZE as usize];

    let allocated = {
        let mut alloc = PAGE_ALLOCATOR.lock();
        alloc.allocate_batch_for_pcp(&mut batch[..needed as usize], flags)
    };

    for i in 0..allocated {
        let frame_num = batch[i];
        loop {
            let head = cache.head.load(Ordering::Acquire);
            {
                let alloc = PAGE_ALLOCATOR.lock();
                if let Some(desc) = unsafe { alloc.frame_desc_mut(frame_num) } {
                    desc.next_free = head;
                }
            }
            if cache
                .head
                .compare_exchange_weak(head, frame_num, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                cache.count.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
}

fn pcp_drain(cpu: usize) {
    if cpu >= MAX_CPUS {
        return;
    }

    let cache = &PER_CPU_CACHES[cpu];
    let current_count = cache.count.load(Ordering::Relaxed);

    if current_count <= PCP_HIGH_WATERMARK {
        return;
    }

    let to_drain = (current_count - PCP_HIGH_WATERMARK / 2).min(PCP_BATCH_SIZE);
    let mut batch = [INVALID_PAGE_FRAME; PCP_BATCH_SIZE as usize];
    let mut drained = 0;

    for i in 0..to_drain as usize {
        loop {
            let head = cache.head.load(Ordering::Acquire);
            if head == INVALID_PAGE_FRAME {
                break;
            }

            let next = {
                let alloc = PAGE_ALLOCATOR.lock();
                unsafe { alloc.frame_desc_mut(head) }
                    .map(|f| f.next_free)
                    .unwrap_or(INVALID_PAGE_FRAME)
            };

            if cache
                .head
                .compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                batch[i] = head;
                cache.count.fetch_sub(1, Ordering::Relaxed);
                drained += 1;
                break;
            }
        }
        if batch[i] == INVALID_PAGE_FRAME {
            break;
        }
    }

    if drained > 0 {
        let mut alloc = PAGE_ALLOCATOR.lock();
        alloc.free_batch_from_pcp(&batch[..drained]);
    }
}

pub fn pcp_drain_all() {
    for cpu in 0..MAX_CPUS {
        let cache = &PER_CPU_CACHES[cpu];
        let mut batch = [INVALID_PAGE_FRAME; PCP_BATCH_SIZE as usize];

        loop {
            let count = cache.count.load(Ordering::Relaxed);
            if count == 0 {
                break;
            }

            let mut drained = 0;
            for slot in batch.iter_mut() {
                *slot = INVALID_PAGE_FRAME;
                loop {
                    let head = cache.head.load(Ordering::Acquire);
                    if head == INVALID_PAGE_FRAME {
                        break;
                    }

                    let next = {
                        let alloc = PAGE_ALLOCATOR.lock();
                        unsafe { alloc.frame_desc_mut(head) }
                            .map(|f| f.next_free)
                            .unwrap_or(INVALID_PAGE_FRAME)
                    };

                    if cache
                        .head
                        .compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        *slot = head;
                        cache.count.fetch_sub(1, Ordering::Relaxed);
                        drained += 1;
                        break;
                    }
                }
                if *slot == INVALID_PAGE_FRAME {
                    break;
                }
            }

            if drained > 0 {
                let mut alloc = PAGE_ALLOCATOR.lock();
                alloc.free_batch_from_pcp(&batch[..drained]);
            } else {
                break;
            }
        }
    }
}

pub fn init_page_allocator(frame_array: *mut c_void, max_frames: u32) -> c_int {
    if frame_array.is_null() || max_frames == 0 {
        panic!("init_page_allocator: Invalid parameters");
    }

    let mut alloc = PAGE_ALLOCATOR.lock();
    alloc.frames = frame_array as *mut PageFrame;
    alloc.total_frames = max_frames;
    alloc.max_supported_frames = max_frames;
    alloc.free_frames = 0;
    alloc.allocated_frames = 0;
    alloc.max_order = PageAllocator::derive_max_order(max_frames);
    alloc.free_lists_reset();

    for i in 0..max_frames {
        if let Some(frame) = unsafe { alloc.frame_desc_mut(i) } {
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
        let region = mm_region_get(i);
        if !region.is_null() {
            let region_ref = unsafe { &*region };
            alloc.seed_region_from_map(region_ref, i as u16);
        }
    }

    drop(alloc);

    PCP_INITIALIZED.store(true, Ordering::Release);

    let alloc = PAGE_ALLOCATOR.lock();
    klog_info!(
        "Page allocator ready: {} pages available (PCP enabled)",
        alloc.free_frames
    );

    0
}

pub fn alloc_page_frames(count: u32, flags: u32) -> PhysAddr {
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
        && (flags & ALLOC_FLAG_NO_PCP) == 0
        && PCP_INITIALIZED.load(Ordering::Acquire);

    let frame_num = if use_pcp {
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
        klog_info!("alloc_page_frames: No suitable block available");
        return PhysAddr::NULL;
    }

    let phys_addr = {
        let alloc = PAGE_ALLOCATOR.lock();
        alloc.frame_to_phys(frame_num)
    };

    if flags & ALLOC_FLAG_ZERO != 0 {
        let span_pages = if use_pcp {
            1
        } else {
            PageAllocator::order_block_pages(order)
        };
        for i in 0..span_pages {
            let page_phys = phys_addr.offset(i as u64 * PAGE_SIZE_4KB);
            if zero_physical_page(page_phys) != 0 {
                free_page_frame(phys_addr);
                return PhysAddr::NULL;
            }
        }
    }

    phys_addr
}

pub fn alloc_page_frame(flags: u32) -> PhysAddr {
    alloc_page_frames(1, flags)
}

pub fn free_page_frame(phys_addr: PhysAddr) -> c_int {
    let frame_num = {
        let alloc = PAGE_ALLOCATOR.lock();
        alloc.phys_to_frame(phys_addr)
    };

    let (is_valid, is_allocated, order, is_pcp_candidate) = {
        let alloc = PAGE_ALLOCATOR.lock();
        if !alloc.is_valid_frame(frame_num) {
            klog_info!("free_page_frame: Invalid physical address");
            return -1;
        }

        let frame = unsafe { alloc.frame_desc_mut(frame_num) }.unwrap();
        let is_alloc = PageAllocator::frame_state_is_allocated(frame.state);
        let ord = frame.order as u32;

        let pcp_ok = ord == 0
            && frame.state == PAGE_FRAME_ALLOCATED
            && PCP_INITIALIZED.load(Ordering::Acquire);

        if !is_alloc {
            return 0;
        }

        if frame.ref_count > 1 {
            frame.ref_count -= 1;
            return 0;
        }

        (true, is_alloc, ord, pcp_ok)
    };

    if !is_valid || !is_allocated {
        return 0;
    }

    if is_pcp_candidate {
        let cpu = get_current_cpu();
        if pcp_try_free(cpu, frame_num) {
            let cache = &PER_CPU_CACHES[cpu];
            if cache.count.load(Ordering::Relaxed) > PCP_HIGH_WATERMARK {
                pcp_drain(cpu);
            }
            return 0;
        }
    }

    let mut alloc = PAGE_ALLOCATOR.lock();
    if let Some(frame) = unsafe { alloc.frame_desc_mut(frame_num) } {
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

    let mut pcp_count = 0u32;
    for cpu in 0..MAX_CPUS {
        pcp_count += PER_CPU_CACHES[cpu].count.load(Ordering::Relaxed);
    }
    let _ = pcp_count;

    unsafe {
        if !total.is_null() {
            *total = alloc.total_frames;
        }
        if !free.is_null() {
            *free = alloc.free_frames;
        }
        if !allocated.is_null() {
            *allocated = alloc.allocated_frames;
        }
    }
}

pub fn get_pcp_stats(cpu: usize, count: *mut u32, allocs: *mut u32, frees: *mut u32) {
    if cpu >= MAX_CPUS {
        return;
    }

    let cache = &PER_CPU_CACHES[cpu];
    unsafe {
        if !count.is_null() {
            *count = cache.count.load(Ordering::Relaxed);
        }
        if !allocs.is_null() {
            *allocs = cache.alloc_count.load(Ordering::Relaxed);
        }
        if !frees.is_null() {
            *frees = cache.free_count.load(Ordering::Relaxed);
        }
    }
}

pub fn page_frame_is_tracked(phys_addr: PhysAddr) -> c_int {
    let alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    (frame_num < alloc.total_frames) as c_int
}

pub fn page_frame_can_free(phys_addr: PhysAddr) -> c_int {
    let alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    if !alloc.is_valid_frame(frame_num) {
        return 0;
    }
    let frame = unsafe { alloc.frame_desc_mut(frame_num) }.unwrap();
    PageAllocator::frame_state_is_allocated(frame.state) as c_int
}

pub fn page_frame_inc_ref(phys_addr: PhysAddr) -> c_int {
    let alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    if !alloc.is_valid_frame(frame_num) {
        return -1;
    }
    let frame = unsafe { alloc.frame_desc_mut(frame_num) }.unwrap();
    if !PageAllocator::frame_state_is_allocated(frame.state) {
        return -1;
    }
    frame.ref_count = frame.ref_count.saturating_add(1);
    frame.ref_count as c_int
}

pub fn page_frame_get_ref(phys_addr: PhysAddr) -> u32 {
    let alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    if !alloc.is_valid_frame(frame_num) {
        return 0;
    }
    let frame = unsafe { alloc.frame_desc_mut(frame_num) }.unwrap();
    frame.ref_count
}

pub fn page_allocator_paint_all(value: u8) {
    let alloc = PAGE_ALLOCATOR.lock();
    if alloc.frames.is_null() {
        return;
    }

    for frame_num in 0..alloc.total_frames {
        let phys_addr = alloc.frame_to_phys(frame_num);
        if let Some(virt_addr) = phys_addr.to_virt_checked() {
            unsafe {
                ptr::write_bytes(virt_addr.as_mut_ptr::<u8>(), value, PAGE_SIZE_4KB as usize);
            }
        }
    }
}

fn zero_physical_page(phys_addr: PhysAddr) -> c_int {
    if phys_addr.is_null() {
        return -1;
    }

    match phys_addr.to_virt_checked() {
        Some(virt) => {
            unsafe {
                ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, PAGE_SIZE_4KB as usize);
            }
            0
        }
        None => -1,
    }
}
