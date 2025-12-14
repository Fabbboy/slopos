#![allow(dead_code)]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use spin::Mutex;

use crate::mm_constants::{
    PAGE_KERNEL_RW, PAGE_SIZE_4KB,
};
use crate::page_alloc::{alloc_page_frame, free_page_frame};
use crate::paging::{map_page_4kb, unmap_page, virt_to_phys};
use crate::memory_layout::{mm_get_kernel_heap_end, mm_get_kernel_heap_start};

unsafe extern "C" {
    fn klog_printf(level: slopos_lib::klog::KlogLevel, fmt: *const c_char, ...) -> c_int;
    fn kernel_panic(msg: *const c_char) -> !;
    fn wl_award_win();
    fn wl_award_loss();
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HeapStats {
    pub total_size: u64,
    pub allocated_size: u64,
    pub free_size: u64,
    pub total_blocks: u32,
    pub allocated_blocks: u32,
    pub free_blocks: u32,
    pub allocation_count: u32,
    pub free_count: u32,
}

#[repr(C)]
struct HeapBlock {
    magic: u32,
    size: u32,
    flags: u32,
    checksum: u32,
    next: *mut HeapBlock,
    prev: *mut HeapBlock,
}

#[derive(Clone, Copy)]
struct FreeList {
    head: *mut HeapBlock,
    count: u32,
    size_class: u32,
}

impl FreeList {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            count: 0,
            size_class: 0,
        }
    }
}

struct KernelHeap {
    start_addr: u64,
    end_addr: u64,
    current_break: u64,
    free_lists: [FreeList; 16],
    stats: HeapStats,
    initialized: bool,
    diagnostics_enabled: bool,
}

unsafe impl Send for KernelHeap {}

impl KernelHeap {
    const fn new() -> Self {
        Self {
            start_addr: 0,
            end_addr: 0,
            current_break: 0,
            free_lists: [FreeList::new(); 16],
            stats: HeapStats {
                total_size: 0,
                allocated_size: 0,
                free_size: 0,
                total_blocks: 0,
                allocated_blocks: 0,
                free_blocks: 0,
                allocation_count: 0,
                free_count: 0,
            },
            initialized: false,
            diagnostics_enabled: true,
        }
    }
}

static KERNEL_HEAP: Mutex<KernelHeap> = Mutex::new(KernelHeap::new());

const MIN_ALLOC_SIZE: u32 = 16;
const MAX_ALLOC_SIZE: u32 = 0x100000;
const BLOCK_MAGIC_ALLOCATED: u32 = 0xDEAD_BEEF;
const BLOCK_MAGIC_FREE: u32 = 0xFEED_FACE;

fn calculate_checksum(block: &HeapBlock) -> u32 {
    block.magic ^ block.size ^ block.flags
}

fn validate_block(block: &HeapBlock) -> bool {
    if block.magic != BLOCK_MAGIC_ALLOCATED && block.magic != BLOCK_MAGIC_FREE {
        return false;
    }
    block.checksum == calculate_checksum(block)
}

fn get_size_class(size: u32) -> usize {
    if size <= 16 { 0 } else if size <= 32 { 1 } else if size <= 64 { 2 } else if size <= 128 { 3 }
    else if size <= 256 { 4 } else if size <= 512 { 5 } else if size <= 1024 { 6 } else if size <= 2048 { 7 }
    else if size <= 4096 { 8 } else if size <= 8192 { 9 } else if size <= 16_384 { 10 } else if size <= 32_768 { 11 }
    else if size <= 65_536 { 12 } else if size <= 131_072 { 13 } else if size <= 262_144 { 14 } else { 15 }
}

fn round_up_size(size: u32) -> u32 {
    if size < MIN_ALLOC_SIZE {
        return MIN_ALLOC_SIZE;
    }
    let mut rounded = MIN_ALLOC_SIZE;
    while rounded < size {
        rounded <<= 1;
    }
    rounded
}

fn block_from_ptr(ptr: *mut u8) -> *mut HeapBlock {
    unsafe { ptr.offset(-(core::mem::size_of::<HeapBlock>() as isize)) as *mut HeapBlock }
}

fn data_from_block(block: *mut HeapBlock) -> *mut u8 {
    unsafe { (block as *mut u8).add(core::mem::size_of::<HeapBlock>()) }
}

fn add_to_free_list(heap: &mut KernelHeap, block: *mut HeapBlock) {
    unsafe {
        if block.is_null() || !validate_block(&*block) {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"add_to_free_list: Invalid block\n\0".as_ptr() as *const c_char,
            );
            return;
        }
        let size_class = get_size_class((*block).size) as usize;
        let list = &mut heap.free_lists[size_class];

        (*block).magic = BLOCK_MAGIC_FREE;
        (*block).flags = 0;
        (*block).checksum = calculate_checksum(&*block);
        (*block).prev = ptr::null_mut();
        (*block).next = list.head;
        if !list.head.is_null() {
            (*list.head).prev = block;
        }
        list.head = block;
        list.count += 1;

        heap.stats.free_blocks += 1;
        heap.stats.allocated_blocks = heap.stats.allocated_blocks.saturating_sub(1);
    }
}

fn remove_from_free_list(heap: &mut KernelHeap, block: *mut HeapBlock) {
    unsafe {
        if block.is_null() || !validate_block(&*block) {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"remove_from_free_list: Invalid block\n\0".as_ptr() as *const c_char,
            );
            return;
        }

        let size_class = get_size_class((*block).size) as usize;
        let list = &mut heap.free_lists[size_class];

        if !(*block).prev.is_null() {
            (*(*block).prev).next = (*block).next;
        } else {
            list.head = (*block).next;
        }
        if !(*block).next.is_null() {
            (*(*block).next).prev = (*block).prev;
        }

        list.count = list.count.saturating_sub(1);
        (*block).magic = BLOCK_MAGIC_ALLOCATED;
        (*block).next = ptr::null_mut();
        (*block).prev = ptr::null_mut();
        (*block).checksum = calculate_checksum(&*block);

        heap.stats.allocated_blocks += 1;
        heap.stats.free_blocks = heap.stats.free_blocks.saturating_sub(1);
    }
}

fn find_free_block(heap: &KernelHeap, size: u32) -> *mut HeapBlock {
    let size_class = get_size_class(size);
    for i in size_class..16 {
        let head = heap.free_lists[i].head;
        if !head.is_null() {
            return head;
        }
    }
    ptr::null_mut()
}

fn expand_heap(heap: &mut KernelHeap, min_size: u32) -> c_int {
    let mut pages_needed = (min_size + PAGE_SIZE_4KB as u32 - 1) / PAGE_SIZE_4KB as u32;
    if pages_needed < 4 {
        pages_needed = 4;
    }

    unsafe {
        klog_printf(
            slopos_lib::klog::KlogLevel::Debug,
            b"Expanding heap by %u pages\n\0".as_ptr() as *const c_char,
            pages_needed,
        );
    }

    let expansion_start = heap.current_break;
    let total_bytes = (pages_needed as u64) * PAGE_SIZE_4KB;
    let mut mapped_pages = 0u32;

    if expansion_start >= heap.end_addr || expansion_start + total_bytes > heap.end_addr {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"expand_heap: Heap growth denied - would exceed heap window\n\0".as_ptr()
                    as *const c_char,
            );
        }
        unsafe { wl_award_loss() };
        return -1;
    }

    for i in 0..pages_needed {
        let phys_page = alloc_page_frame(0);
        if phys_page == 0 {
            unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"expand_heap: Failed to allocate physical page\n\0".as_ptr() as *const c_char); }
            goto_rollback(expansion_start, mapped_pages);
            return -1;
        }
        let virt_page = expansion_start + (i as u64) * PAGE_SIZE_4KB;
        if map_page_4kb(virt_page, phys_page, PAGE_KERNEL_RW) != 0 {
            unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"expand_heap: Failed to map heap page\n\0".as_ptr() as *const c_char); }
            free_page_frame(phys_page);
            goto_rollback(expansion_start, mapped_pages);
            return -1;
        }
        mapped_pages += 1;
    }

    let new_block_addr = expansion_start;
    let new_block_size = total_bytes - core::mem::size_of::<HeapBlock>() as u64;

    let new_block = new_block_addr as *mut HeapBlock;
    unsafe {
        (*new_block).magic = BLOCK_MAGIC_FREE;
        (*new_block).size = new_block_size as u32;
        (*new_block).flags = 0;
        (*new_block).next = ptr::null_mut();
        (*new_block).prev = ptr::null_mut();
        (*new_block).checksum = calculate_checksum(&*new_block);
    }

    heap.current_break += total_bytes;
    heap.stats.total_size += total_bytes;
    heap.stats.free_size += new_block_size;
    add_to_free_list(heap, new_block);
    0
}

fn goto_rollback(expansion_start: u64, mapped_pages: u32) {
    for j in 0..mapped_pages {
        let virt_page = expansion_start + (j as u64) * PAGE_SIZE_4KB;
        let mapped_phys = virt_to_phys(virt_page);
        if mapped_phys != 0 {
            unmap_page(virt_page);
            free_page_frame(mapped_phys);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kmalloc(size: usize) -> *mut c_void {
    let mut heap = KERNEL_HEAP.lock();

    if !heap.initialized {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"kmalloc: Heap not initialized\n\0".as_ptr() as *const c_char); }
        unsafe { wl_award_loss() };
        return ptr::null_mut();
    }

    if size == 0 || size as u32 > MAX_ALLOC_SIZE {
        unsafe { wl_award_loss() };
        return ptr::null_mut();
    }

    let rounded_size = round_up_size(size as u32);
    let total_size = rounded_size + core::mem::size_of::<HeapBlock>() as u32;

    let mut block = find_free_block(&heap, total_size);
    if block.is_null() {
        if expand_heap(&mut heap, total_size) != 0 {
            unsafe { wl_award_loss() };
            return ptr::null_mut();
        }
        block = find_free_block(&heap, total_size);
    }

    if block.is_null() {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"kmalloc: No suitable block found after expansion\n\0".as_ptr() as *const c_char); }
        unsafe { wl_award_loss() };
        return ptr::null_mut();
    }

    remove_from_free_list(&mut heap, block);

    unsafe {
        if (*block).size > total_size + core::mem::size_of::<HeapBlock>() as u32 + MIN_ALLOC_SIZE {
            let new_block = (block as *mut u8).add(core::mem::size_of::<HeapBlock>() + rounded_size as usize) as *mut HeapBlock;
            (*new_block).magic = BLOCK_MAGIC_FREE;
            (*new_block).size = (*block).size - total_size;
            (*new_block).flags = 0;
            (*new_block).next = ptr::null_mut();
            (*new_block).prev = ptr::null_mut();
            (*new_block).checksum = calculate_checksum(&*new_block);

            (*block).size = rounded_size;
            (*block).checksum = calculate_checksum(&*block);

            add_to_free_list(&mut heap, new_block);
        }

        heap.stats.allocated_size += (*block).size as u64;
        heap.stats.free_size = heap.stats.free_size.saturating_sub((*block).size as u64);
        heap.stats.allocation_count += 1;

        wl_award_win();
        data_from_block(block) as *mut c_void
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kzalloc(size: usize) -> *mut c_void {
    let ptr = kmalloc(size);
    if ptr.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::write_bytes(ptr, 0, size);
    }
    ptr
}

#[unsafe(no_mangle)]
pub extern "C" fn kfree(ptr_in: *mut c_void) {
    if ptr_in.is_null() {
        return;
    }

    let mut heap = KERNEL_HEAP.lock();
    if !heap.initialized {
        return;
    }

    let block = block_from_ptr(ptr_in as *mut u8);
    unsafe {
        if block.is_null() || !validate_block(&*block) || (*block).magic != BLOCK_MAGIC_ALLOCATED {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"kfree: Invalid block or double free detected\n\0".as_ptr() as *const c_char,
            );
            wl_award_loss();
            return;
        }

        heap.stats.allocated_size = heap.stats.allocated_size.saturating_sub((*block).size as u64);
        heap.stats.free_size += (*block).size as u64;
        heap.stats.free_count += 1;

        add_to_free_list(&mut heap, block);
        wl_award_win();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn init_kernel_heap() -> c_int {
    let mut heap = KERNEL_HEAP.lock();
    heap.start_addr = mm_get_kernel_heap_start();
    heap.end_addr = mm_get_kernel_heap_end();
    heap.current_break = heap.start_addr;

    for i in 0..16 {
        heap.free_lists[i].head = ptr::null_mut();
        heap.free_lists[i].count = 0;
        heap.free_lists[i].size_class = i as u32;
    }

    heap.stats = HeapStats::default();

    if expand_heap(&mut heap, (PAGE_SIZE_4KB * 4) as u32) != 0 {
        unsafe { kernel_panic(b"Failed to initialize kernel heap\0".as_ptr() as *const c_char) };
    }

    heap.initialized = true;
    unsafe {
        klog_printf(
            slopos_lib::klog::KlogLevel::Debug,
            b"Kernel heap initialized at 0x%llx\n\0".as_ptr() as *const c_char,
            heap.start_addr,
        );
    }

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn get_heap_stats(stats: *mut HeapStats) {
    let heap = KERNEL_HEAP.lock();
    if !stats.is_null() {
        unsafe {
            *stats = heap.stats;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kernel_heap_enable_diagnostics(enable: c_int) {
    let mut heap = KERNEL_HEAP.lock();
    heap.diagnostics_enabled = enable != 0;
}

#[unsafe(no_mangle)]
pub extern "C" fn print_heap_stats() {
    let heap = KERNEL_HEAP.lock();
    unsafe {
        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"=== Kernel Heap Statistics ===\n\0".as_ptr() as *const c_char,
        );
        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Total size: %llu bytes\n\0".as_ptr() as *const c_char,
            heap.stats.total_size,
        );
        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Allocated: %llu bytes\n\0".as_ptr() as *const c_char,
            heap.stats.allocated_size,
        );
        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Free: %llu bytes\n\0".as_ptr() as *const c_char,
            heap.stats.free_size,
        );
        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Allocations: %u\n\0".as_ptr() as *const c_char,
            heap.stats.allocation_count,
        );
        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Frees: %u\n\0".as_ptr() as *const c_char,
            heap.stats.free_count,
        );

        if !heap.diagnostics_enabled {
            return;
        }

        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Free blocks by class:\n\0".as_ptr() as *const c_char,
        );

        let mut total_free_blocks = 0u64;
        let mut largest_free_block = 0u64;
        let thresholds: [u32; 15] = [
            16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144,
        ];

        for i in 0..16 {
            let mut cursor = heap.free_lists[i].head;
            let mut class_count = 0u32;
            while !cursor.is_null() {
                class_count += 1;
                total_free_blocks += 1;
                let size = (*cursor).size as u64;
                if size > largest_free_block {
                    largest_free_block = size;
                }
                cursor = (*cursor).next;
            }

            if class_count == 0 {
                continue;
            }

            if i < 15 {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"  <= %u: %u blocks\n\0".as_ptr() as *const c_char,
                    thresholds[i],
                    class_count,
                );
            } else {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"  > %u: %u blocks\n\0".as_ptr() as *const c_char,
                    thresholds[14],
                    class_count,
                );
            }
        }

        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Total free blocks: %llu\n\0".as_ptr() as *const c_char,
            total_free_blocks,
        );

        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Largest free block: %llu bytes\n\0".as_ptr() as *const c_char,
            largest_free_block,
        );

        if total_free_blocks > 0 {
            let average_free = if heap.stats.free_size > 0 {
                heap.stats.free_size / total_free_blocks
            } else {
                0
            };
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"Average free block: %llu bytes\n\0".as_ptr() as *const c_char,
                average_free,
            );
        }

        if heap.stats.free_size > 0 {
            let mut fragmented_bytes = heap.stats.free_size;
            if largest_free_block < fragmented_bytes {
                fragmented_bytes -= largest_free_block;
            } else {
                fragmented_bytes = 0;
            }

            let fragmentation_percent = if heap.stats.free_size > 0 {
                (fragmented_bytes * 100) / heap.stats.free_size
            } else {
                0
            };

            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"Fragmented bytes: %llu (%llu%%)\n\0".as_ptr() as *const c_char,
                fragmented_bytes,
                fragmentation_percent,
            );
        }
    }
}
