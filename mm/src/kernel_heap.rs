use core::ffi::{c_int, c_void};
use core::ptr;

use slopos_abi::addr::VirtAddr;
use slopos_lib::free_list::{
    BlockHeader, FreeList, HEADER_SIZE, MAGIC_FREE, MIN_BLOCK_SIZE, round_up_pow2, size_class,
    try_split_block,
};
use slopos_lib::{IrqMutex, klog_debug, klog_info};

use crate::memory_layout::{mm_get_kernel_heap_end, mm_get_kernel_heap_start};
use crate::mm_constants::{PAGE_SIZE_4KB, PageFlags};
use crate::page_alloc::{alloc_page_frame, free_page_frame};
use crate::paging::{map_page_4kb, unmap_page, virt_to_phys};

const NUM_SIZE_CLASSES: usize = 16;
const MAX_ALLOC_SIZE: u32 = 0x100000;

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

struct KernelHeap {
    start_addr: u64,
    end_addr: u64,
    current_break: u64,
    free_lists: [FreeList; NUM_SIZE_CLASSES],
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
            free_lists: [FreeList::new(); NUM_SIZE_CLASSES],
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

    fn add_to_free_list(&mut self, block: *mut BlockHeader) {
        unsafe {
            if block.is_null() || !(*block).is_valid() {
                klog_info!("add_to_free_list: Invalid block");
                return;
            }

            let class = size_class((*block).size as usize, NUM_SIZE_CLASSES);
            (*block).mark_free();
            self.free_lists[class].push_front(block);

            self.stats.free_blocks += 1;
            self.stats.allocated_blocks = self.stats.allocated_blocks.saturating_sub(1);
        }
    }

    fn remove_from_free_list(&mut self, block: *mut BlockHeader) {
        unsafe {
            if block.is_null() || !(*block).is_valid() {
                klog_info!("remove_from_free_list: Invalid block");
                return;
            }

            let class = size_class((*block).size as usize, NUM_SIZE_CLASSES);
            self.free_lists[class].remove(block);
            (*block).mark_allocated();

            self.stats.allocated_blocks += 1;
            self.stats.free_blocks = self.stats.free_blocks.saturating_sub(1);
        }
    }

    fn find_free_block(&self, size: usize) -> *mut BlockHeader {
        let start_class = size_class(size, NUM_SIZE_CLASSES);
        for i in start_class..NUM_SIZE_CLASSES {
            let block = self.free_lists[i].find_first_fit(size);
            if !block.is_null() {
                return block;
            }
        }
        ptr::null_mut()
    }
}

static KERNEL_HEAP: IrqMutex<KernelHeap> = IrqMutex::new(KernelHeap::new());

fn expand_heap(heap: &mut KernelHeap, min_size: u32) -> c_int {
    let mut pages_needed = (min_size + PAGE_SIZE_4KB as u32 - 1) / PAGE_SIZE_4KB as u32;
    if pages_needed < 4 {
        pages_needed = 4;
    }

    klog_debug!("Expanding heap by {} pages", pages_needed);

    let expansion_start = heap.current_break;
    let total_bytes = (pages_needed as u64) * PAGE_SIZE_4KB;
    let mut mapped_pages = 0u32;

    if expansion_start >= heap.end_addr || expansion_start + total_bytes > heap.end_addr {
        klog_info!("expand_heap: Heap growth denied - would exceed heap window");
        return -1;
    }

    for i in 0..pages_needed {
        let phys_page = alloc_page_frame(0);
        if phys_page.is_null() {
            klog_info!("expand_heap: Failed to allocate physical page");
            rollback_expansion(expansion_start, mapped_pages);
            return -1;
        }
        let virt_page = expansion_start + (i as u64) * PAGE_SIZE_4KB;
        if map_page_4kb(
            VirtAddr::new(virt_page),
            phys_page,
            PageFlags::KERNEL_RW.bits(),
        ) != 0
        {
            klog_info!("expand_heap: Failed to map heap page");
            free_page_frame(phys_page);
            rollback_expansion(expansion_start, mapped_pages);
            return -1;
        }
        mapped_pages += 1;
    }

    let new_block_addr = expansion_start;
    let new_block_size = (total_bytes as usize) - HEADER_SIZE;
    let new_block = new_block_addr as *mut BlockHeader;

    unsafe {
        BlockHeader::init(new_block, new_block_size as u32, MAGIC_FREE);
    }

    heap.current_break += total_bytes;
    heap.stats.total_size += total_bytes;
    heap.stats.free_size += new_block_size as u64;
    heap.add_to_free_list(new_block);
    0
}

fn rollback_expansion(expansion_start: u64, mapped_pages: u32) {
    for j in 0..mapped_pages {
        let virt_page = expansion_start + (j as u64) * PAGE_SIZE_4KB;
        let mapped_phys = virt_to_phys(VirtAddr::new(virt_page));
        if !mapped_phys.is_null() {
            unmap_page(VirtAddr::new(virt_page));
            free_page_frame(mapped_phys);
        }
    }
}

pub fn kmalloc(size: usize) -> *mut c_void {
    let mut heap = KERNEL_HEAP.lock();

    if !heap.initialized {
        klog_info!("kmalloc: Heap not initialized");
        return ptr::null_mut();
    }

    if size == 0 || size as u32 > MAX_ALLOC_SIZE {
        return ptr::null_mut();
    }

    let rounded_size = round_up_pow2(size, MIN_BLOCK_SIZE);
    let total_size = rounded_size + HEADER_SIZE;

    let mut block = heap.find_free_block(total_size);
    if block.is_null() {
        if expand_heap(&mut heap, total_size as u32) != 0 {
            return ptr::null_mut();
        }
        block = heap.find_free_block(total_size);
    }

    if block.is_null() {
        klog_info!("kmalloc: No suitable block found after expansion");
        return ptr::null_mut();
    }

    heap.remove_from_free_list(block);

    unsafe {
        let split_block = try_split_block(block, rounded_size, MIN_BLOCK_SIZE);
        if !split_block.is_null() {
            heap.add_to_free_list(split_block);
        }

        heap.stats.allocated_size += (*block).size as u64;
        heap.stats.free_size = heap.stats.free_size.saturating_sub((*block).size as u64);
        heap.stats.allocation_count += 1;

        BlockHeader::data_ptr(block) as *mut c_void
    }
}

pub fn kzalloc(size: usize) -> *mut c_void {
    let ptr = kmalloc(size);
    if ptr.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::write_bytes(ptr, 0, size);
    }
    ptr
}

pub fn kfree(ptr_in: *mut c_void) {
    if ptr_in.is_null() {
        return;
    }

    let mut heap = KERNEL_HEAP.lock();
    if !heap.initialized {
        return;
    }

    let block = unsafe { BlockHeader::from_data_ptr(ptr_in as *mut u8) };
    unsafe {
        if block.is_null() || !(*block).is_valid() || !(*block).is_allocated() {
            klog_info!("kfree: Invalid block or double free detected");
            return;
        }

        heap.stats.allocated_size = heap
            .stats
            .allocated_size
            .saturating_sub((*block).size as u64);
        heap.stats.free_size += (*block).size as u64;
        heap.stats.free_count += 1;

        heap.add_to_free_list(block);
    }
}

pub fn init_kernel_heap() -> c_int {
    let mut heap = KERNEL_HEAP.lock();
    heap.start_addr = mm_get_kernel_heap_start();
    heap.end_addr = mm_get_kernel_heap_end();
    heap.current_break = heap.start_addr;

    for i in 0..NUM_SIZE_CLASSES {
        heap.free_lists[i] = FreeList::new();
    }

    heap.stats = HeapStats::default();

    if expand_heap(&mut heap, (PAGE_SIZE_4KB * 4) as u32) != 0 {
        panic!("Failed to initialize kernel heap");
    }

    heap.initialized = true;
    klog_debug!("Kernel heap initialized at 0x{:x}", heap.start_addr);

    0
}

pub fn get_heap_stats(stats: *mut HeapStats) {
    let heap = KERNEL_HEAP.lock();
    if !stats.is_null() {
        unsafe {
            *stats = heap.stats;
        }
    }
}

pub fn kernel_heap_enable_diagnostics(enable: c_int) {
    let mut heap = KERNEL_HEAP.lock();
    heap.diagnostics_enabled = enable != 0;
}

pub fn print_heap_stats() {
    let heap = KERNEL_HEAP.lock();

    klog_info!("=== Kernel Heap Statistics ===");
    klog_info!("Total size: {} bytes", heap.stats.total_size);
    klog_info!("Allocated: {} bytes", heap.stats.allocated_size);
    klog_info!("Free: {} bytes", heap.stats.free_size);
    klog_info!("Allocations: {}", heap.stats.allocation_count);
    klog_info!("Frees: {}", heap.stats.free_count);

    if !heap.diagnostics_enabled {
        return;
    }

    klog_info!("Free blocks by class:");

    let mut total_free_blocks = 0u64;
    let mut largest_free_block = 0u64;
    let thresholds: [u32; 15] = [
        16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144,
    ];

    for i in 0..NUM_SIZE_CLASSES {
        let mut class_count = 0u32;
        unsafe {
            heap.free_lists[i].for_each(|block| {
                class_count += 1;
                total_free_blocks += 1;
                let size = (*block).size as u64;
                if size > largest_free_block {
                    largest_free_block = size;
                }
            });
        }

        if class_count == 0 {
            continue;
        }

        if i < 15 {
            klog_info!("  <= {}: {} blocks", thresholds[i], class_count);
        } else {
            klog_info!("  > {}: {} blocks", thresholds[14], class_count);
        }
    }

    klog_info!("Total free blocks: {}", total_free_blocks);
    klog_info!("Largest free block: {} bytes", largest_free_block);

    if total_free_blocks > 0 {
        let average_free = if heap.stats.free_size > 0 {
            heap.stats.free_size / total_free_blocks
        } else {
            0
        };
        klog_info!("Average free block: {} bytes", average_free);
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

        klog_info!(
            "Fragmented bytes: {} ({}%)",
            fragmented_bytes,
            fragmentation_percent
        );
    }
}

pub unsafe fn kernel_heap_force_unlock() {
    KERNEL_HEAP.force_unlock();
}
