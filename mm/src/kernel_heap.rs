use core::ffi::{c_int, c_void};
use core::mem;
use core::ptr;

use slopos_abi::addr::VirtAddr;
use slopos_sync::IrqMutex;
use slopos_utils::{align_down_u64, align_up_usize, klog_debug, klog_info};

use crate::memory_layout_defs::{KERNEL_HEAP_VBASE, KERNEL_HEAP_VEND};
use crate::page_alloc::{alloc_page_frame, free_page_frame};
use crate::paging::{map_page_4kb, paging_bump_kernel_mapping_gen, unmap_page};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};

const NUM_SIZE_CLASSES: usize = 8;
const SLAB_DEBUG: bool = false;
const MAX_ALLOC_SIZE: usize = 0x100000;
const SLAB_MAGIC: u32 = 0x534C_4142;
const LARGE_MAGIC: u32 = 0x4C_4152_47;
const LARGE_FREE_MAGIC: u32 = 0x4C_4652_45;
const SLAB_POISON_FREED: u8 = 0x6B;
#[allow(dead_code)]
const SLAB_POISON_ALLOC: u8 = 0x5A;
#[allow(dead_code)]
const SLAB_REDZONE_HEAD: u32 = 0xBB_BB_BB_BB;
#[allow(dead_code)]
const SLAB_REDZONE_TAIL: u32 = 0xCC_CC_CC_CC;
const SIZE_CLASSES: [usize; NUM_SIZE_CLASSES] = [16, 32, 64, 128, 256, 512, 1024, 2048];

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

impl HeapStats {
    fn record_slab_alloc(&mut self, size: u64) {
        self.allocated_size = self.allocated_size.saturating_add(size);
        self.allocated_blocks = self.allocated_blocks.saturating_add(1);
        self.free_blocks = self.free_blocks.saturating_sub(1);
        self.allocation_count = self.allocation_count.saturating_add(1);
        self.free_size = self.total_size.saturating_sub(self.allocated_size);
    }

    fn record_large_alloc(&mut self, size: u64) {
        self.allocated_blocks = self.allocated_blocks.saturating_add(1);
        self.allocated_size = self.allocated_size.saturating_add(size);
        self.allocation_count = self.allocation_count.saturating_add(1);
        self.free_size = self.total_size.saturating_sub(self.allocated_size);
    }

    fn record_slab_free(&mut self, size: u64) {
        self.allocated_size = self.allocated_size.saturating_sub(size);
        self.allocated_blocks = self.allocated_blocks.saturating_sub(1);
        self.free_blocks = self.free_blocks.saturating_add(1);
        self.free_count = self.free_count.saturating_add(1);
        self.free_size = self.total_size.saturating_sub(self.allocated_size);
    }

    fn record_large_free(&mut self, size: u64) {
        self.allocated_size = self.allocated_size.saturating_sub(size);
        self.allocated_blocks = self.allocated_blocks.saturating_sub(1);
        self.free_count = self.free_count.saturating_add(1);
        self.free_size = self.total_size.saturating_sub(self.allocated_size);
    }
}

#[repr(C)]
struct SlabHeader {
    magic: u32,
    object_size: u32,
    total_count: u16,
    free_count: u16,
    next: *mut SlabHeader,
    free_list: *mut u8,
}

#[repr(C)]
struct LargeAllocHeader {
    magic: u32,
    pages: u32,
    size: u32,
    reserved: u32,
    next: *mut LargeAllocHeader,
}

#[derive(Clone, Copy)]
struct SlabCache {
    object_size: usize,
    slabs: *mut SlabHeader,
}

impl SlabCache {
    const fn empty() -> Self {
        Self {
            object_size: 0,
            slabs: ptr::null_mut(),
        }
    }
}

struct KernelHeap {
    start_addr: u64,
    end_addr: u64,
    current_break: u64,
    caches: [SlabCache; NUM_SIZE_CLASSES],
    large_free_list: *mut LargeAllocHeader,
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
            caches: [SlabCache::empty(); NUM_SIZE_CLASSES],
            large_free_list: ptr::null_mut(),
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

static KERNEL_HEAP: IrqMutex<KernelHeap> = IrqMutex::new(KernelHeap::new());

fn slab_object_start() -> usize {
    align_up_usize(mem::size_of::<SlabHeader>(), 16)
}

unsafe fn slab_poison_object_body(obj: *mut u8, object_size: usize) {
    let link_bytes = mem::size_of::<*mut u8>();
    if object_size > link_bytes {
        unsafe {
            ptr::write_bytes(
                obj.add(link_bytes),
                SLAB_POISON_FREED,
                object_size.saturating_sub(link_bytes),
            );
        }
    }
}

fn size_class_index(size: usize) -> Option<usize> {
    for (idx, class) in SIZE_CLASSES.iter().enumerate() {
        if size <= *class {
            return Some(idx);
        }
    }
    None
}

fn map_heap_pages(heap: &mut KernelHeap, pages: u32) -> Option<u64> {
    if pages == 0 {
        return None;
    }

    let total_bytes = pages as u64 * PAGE_SIZE_4KB;
    if heap.current_break == 0 || heap.current_break + total_bytes > heap.end_addr {
        return None;
    }

    let start = heap.current_break;
    let mut mapped_pages = 0u32;

    for i in 0..pages {
        let phys_page = alloc_page_frame(0);
        if phys_page.is_null() {
            rollback_mapping(start, mapped_pages);
            return None;
        }
        let virt_page = start + (i as u64) * PAGE_SIZE_4KB;
        if map_page_4kb(
            VirtAddr::new(virt_page),
            phys_page,
            PageFlags::KERNEL_RW.bits(),
        ) != 0
        {
            free_page_frame(phys_page);
            rollback_mapping(start, mapped_pages);
            return None;
        }
        mapped_pages += 1;
    }

    heap.current_break += total_bytes;
    heap.stats.total_size = heap.stats.total_size.saturating_add(total_bytes);
    heap.stats.free_size = heap
        .stats
        .total_size
        .saturating_sub(heap.stats.allocated_size);
    paging_bump_kernel_mapping_gen();

    Some(start)
}

fn rollback_mapping(start: u64, mapped_pages: u32) {
    for i in 0..mapped_pages {
        let virt_page = start + (i as u64) * PAGE_SIZE_4KB;
        let phys = unmap_page(VirtAddr::new(virt_page));
        if !phys.is_null() {
            free_page_frame(phys);
        }
    }
}

fn slab_build_free_list(base: *mut u8, object_size: usize, total_count: usize) -> *mut u8 {
    let mut head: *mut u8 = ptr::null_mut();
    let mut current: *mut u8 = ptr::null_mut();

    for i in 0..total_count {
        let obj = unsafe { base.add(i * object_size) };
        if head.is_null() {
            head = obj;
            current = obj;
        } else {
            unsafe { *(current as *mut *mut u8) = obj };
            current = obj;
        }
    }

    if !current.is_null() {
        unsafe { *(current as *mut *mut u8) = ptr::null_mut() };
    }

    if SLAB_DEBUG {
        for i in 0..total_count {
            let obj = unsafe { base.add(i * object_size) };
            unsafe { slab_poison_object_body(obj, object_size) };
        }
    }

    head
}

fn slab_create(heap: &mut KernelHeap, object_size: usize) -> *mut SlabHeader {
    let start = slab_object_start();
    if start >= PAGE_SIZE_4KB as usize {
        return ptr::null_mut();
    }

    let available = PAGE_SIZE_4KB as usize - start;
    let total_count = available / object_size;
    if total_count == 0 {
        return ptr::null_mut();
    }

    let slab_addr = match map_heap_pages(heap, 1) {
        Some(addr) => addr,
        None => return ptr::null_mut(),
    };

    let header = slab_addr as *mut SlabHeader;
    let data_base = unsafe { (slab_addr as *mut u8).add(start) };
    let free_list = slab_build_free_list(data_base, object_size, total_count);

    if SLAB_DEBUG {
        for i in 0..total_count {
            let obj = unsafe { data_base.add(i * object_size) };
            unsafe { slab_poison_object_body(obj, object_size) };
        }
    }

    unsafe {
        (*header).magic = SLAB_MAGIC;
        (*header).object_size = object_size as u32;
        (*header).total_count = total_count as u16;
        (*header).free_count = total_count as u16;
        (*header).next = ptr::null_mut();
        (*header).free_list = free_list;
    }

    heap.stats.total_blocks = heap.stats.total_blocks.saturating_add(total_count as u32);
    heap.stats.free_blocks = heap.stats.free_blocks.saturating_add(total_count as u32);

    header
}

fn slab_alloc_from_cache(heap: &mut KernelHeap, idx: usize) -> *mut c_void {
    let cache_ptr = &mut heap.caches[idx] as *mut SlabCache;
    unsafe {
        let mut slab = (*cache_ptr).slabs;
        while !slab.is_null() {
            if (*slab).free_count > 0 {
                let obj = (*slab).free_list;
                if obj.is_null() {
                    return ptr::null_mut();
                }
                let next = *(obj as *mut *mut u8);
                let slab_start = slab as usize;
                let slab_end = slab_start + PAGE_SIZE_4KB as usize;
                if !next.is_null() {
                    let next_addr = next as usize;
                    if next_addr < slab_start || next_addr >= slab_end {
                        klog_info!(
                            "slab_alloc: corrupt next ptr 0x{:x} in obj 0x{:x}, slab [0x{:x}..0x{:x}], obj_size={}",
                            next_addr,
                            obj as usize,
                            slab_start,
                            slab_end,
                            (*slab).object_size
                        );
                        // Sever the corrupt chain — lose some objects but stay alive.
                        (*slab).free_list = ptr::null_mut();
                        (*slab).free_count = 0;
                        return obj as *mut c_void;
                    }
                }

                if SLAB_DEBUG {
                    let _ = (SLAB_REDZONE_HEAD, SLAB_REDZONE_TAIL);
                    let object_size = (*slab).object_size as usize;
                    let body_offset = mem::size_of::<*mut u8>();
                    if object_size > body_offset {
                        let body_len = object_size - body_offset;
                        let body = obj.add(body_offset);
                        let mut corrupt_off: Option<usize> = None;
                        let mut corrupt_byte: u8 = 0;

                        for off in 0..body_len {
                            let actual = *body.add(off);
                            if actual != SLAB_POISON_FREED {
                                corrupt_off = Some(off);
                                corrupt_byte = actual;
                                break;
                            }
                        }

                        if corrupt_off.is_some() {
                            klog_info!(
                                "slab_alloc: POISON CHECK FAILED at 0x{:x}, expected 0x{:02X} found 0x{:02X}, obj_size={}",
                                obj as usize,
                                SLAB_POISON_FREED,
                                corrupt_byte,
                                object_size
                            );
                            klog_info!(
                                "slab_alloc: first16={:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
                                *obj.add(0),
                                *obj.add(1),
                                *obj.add(2),
                                *obj.add(3),
                                *obj.add(4),
                                *obj.add(5),
                                *obj.add(6),
                                *obj.add(7),
                                *obj.add(8),
                                *obj.add(9),
                                *obj.add(10),
                                *obj.add(11),
                                *obj.add(12),
                                *obj.add(13),
                                *obj.add(14),
                                *obj.add(15)
                            );
                        }
                    }
                }

                (*slab).free_list = next;
                (*slab).free_count = (*slab).free_count.saturating_sub(1);
                heap.stats.record_slab_alloc((*slab).object_size as u64);

                return obj as *mut c_void;
            }
            slab = (*slab).next;
        }

        let object_size = (*cache_ptr).object_size;
        let new_slab = slab_create(heap, object_size);
        if new_slab.is_null() {
            return ptr::null_mut();
        }
        (*new_slab).next = (*cache_ptr).slabs;
        (*cache_ptr).slabs = new_slab;
    }

    slab_alloc_from_cache(heap, idx)
}

fn alloc_large(heap: &mut KernelHeap, size: usize) -> *mut c_void {
    let header_size = align_up_usize(mem::size_of::<LargeAllocHeader>(), 16);
    let total = size.saturating_add(header_size);
    let pages = align_up_usize(total, PAGE_SIZE_4KB as usize) / PAGE_SIZE_4KB as usize;

    if pages == 0 {
        return ptr::null_mut();
    }

    let mut prev: *mut LargeAllocHeader = ptr::null_mut();
    let mut current = heap.large_free_list;
    while !current.is_null() {
        unsafe {
            if (*current).pages as usize >= pages {
                if prev.is_null() {
                    heap.large_free_list = (*current).next;
                } else {
                    (*prev).next = (*current).next;
                }
                let base = current as u64;

                if SLAB_DEBUG {
                    let body = (base as *mut u8).add(header_size);
                    let check_len = size
                        .min(((*current).pages as usize) * PAGE_SIZE_4KB as usize - header_size);
                    let mut corrupt_off: Option<usize> = None;
                    let mut corrupt_byte: u8 = 0;
                    for off in 0..check_len.min(64) {
                        let actual = *body.add(off);
                        if actual != SLAB_POISON_FREED {
                            corrupt_off = Some(off);
                            corrupt_byte = actual;
                            break;
                        }
                    }
                    if let Some(off) = corrupt_off {
                        klog_info!(
                            "alloc_large: POISON CHECK FAILED at 0x{:x}+{}, expected 0x{:02X} found 0x{:02X}, size={}, pages={}",
                            base,
                            off,
                            SLAB_POISON_FREED,
                            corrupt_byte,
                            size,
                            (*current).pages
                        );
                    }
                }

                (*current).magic = LARGE_MAGIC;
                (*current).size = size as u32;
                (*current).next = ptr::null_mut();
                heap.stats.record_large_alloc(size as u64);
                return (base as *mut u8).add(header_size) as *mut c_void;
            }
            prev = current;
            current = (*current).next;
        }
    }

    let base = match map_heap_pages(heap, pages as u32) {
        Some(addr) => addr,
        None => return ptr::null_mut(),
    };

    let header = base as *mut LargeAllocHeader;
    unsafe {
        (*header).magic = LARGE_MAGIC;
        (*header).pages = pages as u32;
        (*header).size = size as u32;
        (*header).reserved = 0;
        (*header).next = ptr::null_mut();
    }

    heap.stats.total_blocks = heap.stats.total_blocks.saturating_add(1);
    heap.stats.record_large_alloc(size as u64);

    unsafe { (base as *mut u8).add(header_size) as *mut c_void }
}

fn free_large(heap: &mut KernelHeap, base: u64) -> c_int {
    let header = base as *mut LargeAllocHeader;
    unsafe {
        if (*header).magic != LARGE_MAGIC {
            return -1;
        }
        let size = (*header).size as u64;
        (*header).magic = LARGE_FREE_MAGIC;
        (*header).next = heap.large_free_list;
        heap.large_free_list = header;
        heap.stats.record_large_free(size);

        if SLAB_DEBUG {
            let hdr_sz = align_up_usize(mem::size_of::<LargeAllocHeader>(), 16);
            let total_bytes = ((*header).pages as usize) * PAGE_SIZE_4KB as usize;
            if total_bytes > hdr_sz {
                let body = (base as *mut u8).add(hdr_sz);
                let body_len = total_bytes - hdr_sz;
                ptr::write_bytes(body, SLAB_POISON_FREED, body_len);
            }
        }
    }

    0
}

fn slab_free(heap: &mut KernelHeap, ptr_in: *mut c_void) -> c_int {
    let base = align_down_u64(ptr_in as u64, PAGE_SIZE_4KB) as *mut SlabHeader;
    if base.is_null() {
        return -1;
    }

    unsafe {
        if (*base).magic != SLAB_MAGIC {
            return -1;
        }

        let object_size = (*base).object_size as usize;
        let start = slab_object_start();
        let object_base = (base as usize).saturating_add(start);
        let object_end = (base as usize).saturating_add(PAGE_SIZE_4KB as usize);
        let ptr_addr = ptr_in as usize;

        if ptr_addr < object_base || ptr_addr >= object_end {
            return -1;
        }

        let offset = ptr_addr - object_base;
        if offset % object_size != 0 {
            return -1;
        }

        let slab_start = base as usize;
        let slab_end = slab_start + PAGE_SIZE_4KB as usize;
        let mut current = (*base).free_list;
        while !current.is_null() {
            let cur_addr = current as usize;
            if cur_addr < slab_start || cur_addr >= slab_end {
                // Free-list pointer escaped the slab page — metadata is corrupt.
                // Break the walk to avoid dereferencing into unmapped memory.
                klog_info!(
                    "slab_free: corrupt free-list ptr 0x{:x} outside slab [0x{:x}..0x{:x}], obj_size={}",
                    cur_addr,
                    slab_start,
                    slab_end,
                    object_size
                );
                break;
            }
            if cur_addr == ptr_addr {
                return -1;
            }
            current = *(current as *mut *mut u8);
        }

        *(ptr_in as *mut *mut u8) = (*base).free_list;
        (*base).free_list = ptr_in as *mut u8;
        (*base).free_count = (*base).free_count.saturating_add(1);

        if SLAB_DEBUG {
            let _ = (SLAB_REDZONE_HEAD, SLAB_REDZONE_TAIL);
            slab_poison_object_body(ptr_in as *mut u8, object_size);
        }

        heap.stats.record_slab_free((*base).object_size as u64);
    }

    0
}

pub fn kmalloc(size: usize) -> *mut c_void {
    let mut heap = KERNEL_HEAP.lock();

    if !heap.initialized {
        klog_info!("kmalloc: Heap not initialized");
        return ptr::null_mut();
    }

    if size == 0 || size > MAX_ALLOC_SIZE {
        return ptr::null_mut();
    }

    let rounded_size = align_up_usize(size, 16);
    let result = if let Some(idx) = size_class_index(rounded_size) {
        slab_alloc_from_cache(&mut heap, idx)
    } else {
        alloc_large(&mut heap, rounded_size)
    };

    result
}

pub fn kzalloc(size: usize) -> *mut c_void {
    let ptr_out = kmalloc(size);
    if ptr_out.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::write_bytes(ptr_out, 0, size);
    }
    ptr_out
}

pub fn kfree(ptr_in: *mut c_void) {
    if ptr_in.is_null() {
        return;
    }

    let mut heap = KERNEL_HEAP.lock();
    if !heap.initialized {
        return;
    }

    let base = align_down_u64(ptr_in as u64, PAGE_SIZE_4KB);
    if base < heap.start_addr || base >= heap.current_break {
        klog_info!(
            "kfree: ptr 0x{:x} outside heap [0x{:x}..0x{:x}]",
            ptr_in as u64,
            heap.start_addr,
            heap.current_break
        );
        return;
    }
    let slab_result = slab_free(&mut heap, ptr_in);
    if slab_result == 0 {
        return;
    }

    let large_result = free_large(&mut heap, base);
    if large_result == 0 {
        return;
    }

    let magic_at_base = unsafe { *(base as *const u32) };
    klog_info!(
        "kfree: no owner for ptr 0x{:x} base 0x{:x} magic=0x{:08x} (slab_rc={} large_rc={})",
        ptr_in as u64,
        base,
        magic_at_base,
        slab_result,
        large_result
    );
}

/// Minimum pages required for soft reboot coherency fix.
/// See documentation in `init_kernel_heap()` for details.
pub const HEAP_WARMUP_PAGES: u32 = 4;

pub fn init_kernel_heap() -> c_int {
    let mut heap = KERNEL_HEAP.lock();
    heap.start_addr = KERNEL_HEAP_VBASE;
    heap.end_addr = KERNEL_HEAP_VEND;
    heap.current_break = heap.start_addr;

    for (idx, size) in SIZE_CLASSES.iter().enumerate() {
        heap.caches[idx] = SlabCache {
            object_size: *size,
            slabs: ptr::null_mut(),
        };
    }

    heap.stats = HeapStats::default();
    heap.large_free_list = ptr::null_mut();

    // ============================================================================
    // SOFT REBOOT COHERENCY FIX - DO NOT REMOVE
    // ============================================================================
    //
    // After soft reboot (PS/2 0xFE reset), x86 paging structure caches may retain
    // stale entries from the previous boot. Limine creates fresh page tables, but
    // the CPU's internal paging structure caches aren't automatically coherent.
    //
    // This causes framebuffer performance to degrade from ~60 FPS to ~1 FPS because:
    // 1. Stale paging structure cache entries point to old page table locations
    // 2. PAT (Page Attribute Table) settings for Write-Combining aren't applied
    // 3. Framebuffer writes fall back to uncached mode (~37,000 cycles/pixel)
    //
    // The fix requires BOTH:
    // - ≥2 physical frame allocations: Forces buddy allocator metadata coherency
    //   via read-after-write serialization on the bitmap/free list structures
    // - ≥1 page mapping: Forces page table walks that populate paging structure
    //   caches with fresh entries from Limine's new page tables
    //
    // `map_heap_pages(4)` satisfies both requirements (4 allocs + 4 maps).
    // Experiments confirmed 2 pages minimum works, but 4 provides safety margin.
    //
    // References:
    // - Intel Application Note 317080-002: "TLBs, Paging-Structure Caches, and
    //   Their Invalidation"
    // - https://blog.stuffedcow.net/2015/08/pagewalk-coherence/
    //
    // WARNING: Removing or reducing this below 2 pages WILL cause framebuffer
    // performance regression after soft reboot. See test_heap_warmup_pages_minimum().
    // ============================================================================
    if map_heap_pages(&mut heap, HEAP_WARMUP_PAGES).is_none() {
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

    for cache in heap.caches.iter() {
        if cache.object_size == 0 {
            continue;
        }

        let mut total = 0u32;
        let mut free = 0u32;
        let mut slab = cache.slabs;
        unsafe {
            while !slab.is_null() {
                total += (*slab).total_count as u32;
                free += (*slab).free_count as u32;
                slab = (*slab).next;
            }
        }

        if total > 0 {
            klog_info!(
                "Slab {}B: free {} / total {}",
                cache.object_size,
                free,
                total
            );
        }
    }
}

pub unsafe fn kernel_heap_force_unlock() {
    KERNEL_HEAP.force_unlock();
}

/// Force-unlock the kernel heap AND mark it as poisoned.
/// Called from panic recovery to signal that heap metadata may be
/// inconsistent. Check `kernel_heap_is_poisoned()` before trusting state.
pub unsafe fn kernel_heap_poison_unlock() {
    KERNEL_HEAP.poison_unlock();
}

/// Returns true if the kernel heap was force-unlocked during panic recovery.
pub fn kernel_heap_is_poisoned() -> bool {
    KERNEL_HEAP.is_poisoned()
}

/// Clear the kernel heap's poisoned state after reinitialization.
pub fn kernel_heap_clear_poison() {
    KERNEL_HEAP.clear_poison();
}
