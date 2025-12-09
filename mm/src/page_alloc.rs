#![allow(dead_code)]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use spin::Mutex;

use crate::mm_constants::{
    ENTRIES_PER_PAGE_TABLE, PAGE_PRESENT, PAGE_SIZE_4KB,
};
use crate::memory_reservations::{
    mm_region_count, mm_region_get, MmRegion, MmRegionKind, MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT,
};
use crate::phys_virt::{mm_phys_to_virt, mm_zero_physical_page};

// Allocation flags (mirrors page_alloc.h)
pub const ALLOC_FLAG_ZERO: u32 = 0x01;
pub const ALLOC_FLAG_DMA: u32 = 0x02;
pub const ALLOC_FLAG_KERNEL: u32 = 0x04;
pub const ALLOC_FLAG_ORDER_SHIFT: u32 = 8;
pub const ALLOC_FLAG_ORDER_MASK: u32 = 0x1F << ALLOC_FLAG_ORDER_SHIFT;

// Page frame states
const PAGE_FRAME_FREE: u8 = 0x00;
const PAGE_FRAME_ALLOCATED: u8 = 0x01;
const PAGE_FRAME_RESERVED: u8 = 0x02;
const PAGE_FRAME_KERNEL: u8 = 0x03;
const PAGE_FRAME_DMA: u8 = 0x04;

const INVALID_PAGE_FRAME: u32 = 0xFFFF_FFFF;
const MAX_ORDER: u32 = 24;
const INVALID_REGION_ID: u16 = 0xFFFF;

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

    fn phys_to_frame(&self, phys_addr: u64) -> u32 {
        (phys_addr >> 12) as u32
    }

    fn frame_to_phys(&self, frame_num: u32) -> u64 {
        (frame_num as u64) << 12
    }

    fn is_valid_frame(&self, frame_num: u32) -> bool {
        frame_num < self.total_frames
    }

    unsafe fn frame_desc_mut(&self, frame_num: u32) -> Option<&'static mut PageFrame> {
        if !self.is_valid_frame(frame_num) || self.frames.is_null() {
            return None;
        }
        Some(&mut *self.frames.add(frame_num as usize))
    }

    fn frame_region_id(&self, frame_num: u32) -> u16 {
        unsafe { self.frame_desc_mut(frame_num) }.map(|f| f.region_id).unwrap_or(INVALID_REGION_ID)
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
        matches!(state, PAGE_FRAME_ALLOCATED | PAGE_FRAME_KERNEL | PAGE_FRAME_DMA)
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
        let head = &mut self.free_lists[order as usize];
        let mut prev = INVALID_PAGE_FRAME;
        let mut current = *head;

        while current != INVALID_PAGE_FRAME {
            if current == target_frame {
                let next = unsafe { self.frame_desc_mut(current) }.map(|f| f.next_free).unwrap_or(INVALID_PAGE_FRAME);
                if prev == INVALID_PAGE_FRAME {
                    *head = next;
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
        let phys = self.frame_to_phys(frame_num);
        let span = (Self::order_block_pages(order) as u64) * PAGE_SIZE_4KB;
        if flags & ALLOC_FLAG_DMA != 0 && phys + span > DMA_MEMORY_LIMIT {
            return false;
        }
        true
    }

    fn free_list_take_matching(&mut self, order: u32, flags: u32) -> u32 {
        let head = &mut self.free_lists[order as usize];
        let mut prev = INVALID_PAGE_FRAME;
        let mut current = *head;

        while current != INVALID_PAGE_FRAME {
            if self.block_meets_flags(current, order, flags) {
                let next = unsafe { self.frame_desc_mut(current) }.map(|f| f.next_free).unwrap_or(INVALID_PAGE_FRAME);
                if prev == INVALID_PAGE_FRAME {
                    *head = next;
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
                .map(|b| b.state == PAGE_FRAME_FREE && b.order == curr_order as u16 && b.region_id == region_id)
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

            // split down
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

        let mut start_frame = self.phys_to_frame(aligned_start);
        let mut end_frame = self.phys_to_frame(aligned_end);
        if start_frame >= self.total_frames {
            return;
        }
        if end_frame > self.total_frames {
            end_frame = self.total_frames;
        }

        let mut remaining = end_frame - start_frame;
        let mut frame = start_frame;
        let seeded_id = if region_id == INVALID_REGION_ID { 0 } else { region_id };

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

static PAGE_ALLOCATOR: Mutex<PageAllocator> = Mutex::new(PageAllocator::new());

extern "C" {
    fn kernel_panic(msg: *const c_char) -> !;
    fn klog_printf(level: slopos_lib::klog::KlogLevel, fmt: *const c_char, ...) -> c_int;
}

const DMA_MEMORY_LIMIT: u64 = 0x0100_0000;

fn align_down_u64(value: u64, alignment: u64) -> u64 {
    if alignment == 0 {
        value
    } else {
        value & !(alignment - 1)
    }
}

fn align_up_u64(value: u64, alignment: u64) -> u64 {
    if alignment == 0 {
        value
    } else {
        (value + alignment - 1) & !(alignment - 1)
    }
}

#[no_mangle]
pub extern "C" fn init_page_allocator(frame_array: *mut c_void, max_frames: u32) -> c_int {
    if frame_array.is_null() || max_frames == 0 {
        unsafe { kernel_panic(b"init_page_allocator: Invalid parameters\0".as_ptr() as *const c_char) };
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

    unsafe {
        klog_printf(
            slopos_lib::klog::KlogLevel::Debug,
            b"Page frame allocator initialized with %u frame descriptors (max order %u)\n\0".as_ptr()
                as *const c_char,
            max_frames,
            alloc.max_order,
        );
    }

    0
}

#[no_mangle]
pub extern "C" fn finalize_page_allocator() -> c_int {
    let mut alloc = PAGE_ALLOCATOR.lock();
    alloc.free_lists_reset();
    alloc.free_frames = 0;
    alloc.allocated_frames = 0;

    let region_count = mm_region_count();
    for i in 0..region_count {
        let region = unsafe { mm_region_get(i) };
        if !region.is_null() {
            let region_ref = unsafe { &*region };
            alloc.seed_region_from_map(region_ref, i as u16);
        }
    }

    unsafe {
        klog_printf(
            slopos_lib::klog::KlogLevel::Debug,
            b"Page allocator ready: %u pages available\n\0".as_ptr() as *const c_char,
            alloc.free_frames,
        );
    }

    0
}

#[no_mangle]
pub extern "C" fn alloc_page_frames(count: u32, flags: u32) -> u64 {
    if count == 0 {
        return 0;
    }

    let mut order = 0;
    let mut pages = 1;
    while pages < count && order < MAX_ORDER {
        pages <<= 1;
        order += 1;
    }

    let mut alloc = PAGE_ALLOCATOR.lock();
    let flag_order = alloc.flags_to_order(flags);
    if flag_order > order {
        order = flag_order;
    }

    let frame_num = alloc.allocate_block(order, flags);
    if frame_num == INVALID_PAGE_FRAME {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"alloc_page_frames: No suitable block available\n\0".as_ptr() as *const c_char,
            );
        }
        return 0;
    }

    let phys_addr = alloc.frame_to_phys(frame_num);
    drop(alloc);

    if flags & ALLOC_FLAG_ZERO != 0 {
        let span_pages = PageAllocator::order_block_pages(order);
        for i in 0..span_pages {
            if unsafe { mm_zero_physical_page(phys_addr + (i as u64 * PAGE_SIZE_4KB)) } != 0 {
                free_page_frame(phys_addr);
                return 0;
            }
        }
    }

    phys_addr
}

#[no_mangle]
pub extern "C" fn alloc_page_frame(flags: u32) -> u64 {
    alloc_page_frames(1, flags)
}

#[no_mangle]
pub extern "C" fn free_page_frame(phys_addr: u64) -> c_int {
    let mut alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);

    if !alloc.is_valid_frame(frame_num) {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"free_page_frame: Invalid physical address\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    let frame = unsafe { alloc.frame_desc_mut(frame_num) }.unwrap();
    if !PageAllocator::frame_state_is_allocated(frame.state) {
        return 0;
    }

    if frame.ref_count > 1 {
        frame.ref_count -= 1;
        return 0;
    }

    let order = frame.order as u32;
    let pages = PageAllocator::order_block_pages(order);

    frame.ref_count = 0;
    frame.flags = 0;
    frame.state = PAGE_FRAME_FREE;

    alloc.allocated_frames = alloc.allocated_frames.saturating_sub(pages);
    alloc.insert_block_coalescing(frame_num, order);
    0
}

#[no_mangle]
pub extern "C" fn page_allocator_descriptor_size() -> usize {
    core::mem::size_of::<PageFrame>()
}

#[no_mangle]
pub extern "C" fn page_allocator_max_supported_frames() -> u32 {
    PAGE_ALLOCATOR.lock().max_supported_frames
}

#[no_mangle]
pub extern "C" fn get_page_allocator_stats(total: *mut u32, free: *mut u32, allocated: *mut u32) {
    let alloc = PAGE_ALLOCATOR.lock();
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

#[no_mangle]
pub extern "C" fn page_frame_is_tracked(phys_addr: u64) -> c_int {
    let alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    (frame_num < alloc.total_frames) as c_int
}

#[no_mangle]
pub extern "C" fn page_frame_can_free(phys_addr: u64) -> c_int {
    let alloc = PAGE_ALLOCATOR.lock();
    let frame_num = alloc.phys_to_frame(phys_addr);
    if !alloc.is_valid_frame(frame_num) {
        return 0;
    }
    let frame = unsafe { alloc.frame_desc_mut(frame_num) }.unwrap();
    PageAllocator::frame_state_is_allocated(frame.state) as c_int
}

#[no_mangle]
pub extern "C" fn page_allocator_paint_all(value: u8) {
    let alloc = PAGE_ALLOCATOR.lock();
    if alloc.frames.is_null() {
        return;
    }

    for frame_num in 0..alloc.total_frames {
        let phys_addr = alloc.frame_to_phys(frame_num);
        let virt_addr = mm_phys_to_virt(phys_addr);
        if virt_addr == 0 {
            continue;
        }
        unsafe {
            ptr::write_bytes(virt_addr as *mut u8, value, PAGE_SIZE_4KB as usize);
        }
    }
}

