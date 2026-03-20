use core::cell::SyncUnsafeCell;
use core::ffi::c_void;
use core::ptr;

use super::free_list::{
    BlockHeader, FreeList, HEADER_SIZE, MAGIC_ALLOCATED, MAGIC_FREE, MIN_BLOCK_SIZE,
    try_split_block,
};
use crate::pal::raw::{syscall2, syscall6};
use crate::pal::syscall::sys_brk;
use slopos_abi::syscall::{
    MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE, SYSCALL_MMAP, SYSCALL_MUNMAP,
};

use slopos_abi::alignment::align_up_usize;

pub const ALIGNMENT: usize = 16;
const INITIAL_HEAP_SIZE: usize = 64 * 1024;
const EXTEND_MIN_SIZE: usize = 64 * 1024;
const MMAP_THRESHOLD: usize = 128 * 1024;
const MMAP_FLAG: u32 = 1;
const PAGE_SIZE: usize = 4096;

#[repr(transparent)]
struct SyncBlockPtr(*mut BlockHeader);
unsafe impl Sync for SyncBlockPtr {}

#[repr(transparent)]
struct SyncBytePtr(*mut u8);
unsafe impl Sync for SyncBytePtr {}

#[repr(transparent)]
struct SyncFreeList(FreeList);
unsafe impl Sync for SyncFreeList {}

static HEAP_START: SyncUnsafeCell<SyncBlockPtr> =
    SyncUnsafeCell::new(SyncBlockPtr(ptr::null_mut()));
static HEAP_END: SyncUnsafeCell<SyncBytePtr> = SyncUnsafeCell::new(SyncBytePtr(ptr::null_mut()));
static FREE_LIST: SyncUnsafeCell<SyncFreeList> = SyncUnsafeCell::new(SyncFreeList(FreeList::new()));

unsafe fn init_heap() {
    if !(*HEAP_START.get()).0.is_null() {
        return;
    }

    let current_brk = sys_brk(ptr::null_mut()) as *mut u8;
    if current_brk.is_null() || current_brk as usize == usize::MAX {
        return;
    }

    let new_brk = current_brk.add(INITIAL_HEAP_SIZE);
    let result = sys_brk(new_brk as *mut c_void) as *mut u8;

    if result != new_brk {
        return;
    }

    (*HEAP_START.get()).0 = current_brk as *mut BlockHeader;
    (*HEAP_END.get()).0 = new_brk;

    let first_block = (*HEAP_START.get()).0;
    BlockHeader::init(
        first_block,
        (INITIAL_HEAP_SIZE - HEADER_SIZE) as u32,
        MAGIC_FREE,
    );
    (*FREE_LIST.get()).0.push_front(first_block);
}

unsafe fn extend_heap(min_size: usize) -> *mut BlockHeader {
    let extend_size = align_up_usize(min_size + HEADER_SIZE, ALIGNMENT).max(EXTEND_MIN_SIZE);
    let new_brk = (*HEAP_END.get()).0.add(extend_size);
    let result = sys_brk(new_brk as *mut c_void) as *mut u8;

    if result != new_brk {
        return ptr::null_mut();
    }

    let new_block = (*HEAP_END.get()).0 as *mut BlockHeader;
    BlockHeader::init(new_block, (extend_size - HEADER_SIZE) as u32, MAGIC_FREE);
    (*FREE_LIST.get()).0.push_front(new_block);
    (*HEAP_END.get()).0 = new_brk;

    new_block
}

unsafe fn try_coalesce_forward(block: *mut BlockHeader) {
    let block_end = BlockHeader::block_end(block);
    if block_end >= (*HEAP_END.get()).0 {
        return;
    }

    let next = block_end as *mut BlockHeader;
    if !(*next).is_valid() || !(*next).is_free() {
        return;
    }

    (*FREE_LIST.get()).0.remove(next);
    (*block).size += HEADER_SIZE as u32 + (*next).size;
    (*block).update_checksum();
}

unsafe fn alloc_mmap(size: usize) -> *mut c_void {
    let total = align_up_usize(size + HEADER_SIZE, PAGE_SIZE);
    let ret = syscall6(
        SYSCALL_MMAP,
        0,
        total as u64,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        (-1i64) as u64,
        0,
    );
    match crate::demux(ret) {
        Ok(addr) => {
            let block = addr as *mut BlockHeader;
            BlockHeader::init(block, (total - HEADER_SIZE) as u32, MAGIC_ALLOCATED);
            (*block).flags = MMAP_FLAG;
            (*block).update_checksum();
            BlockHeader::data_ptr(block) as *mut c_void
        }
        Err(_) => ptr::null_mut(),
    }
}

pub fn alloc(size: usize) -> *mut c_void {
    if size == 0 {
        return ptr::null_mut();
    }

    unsafe {
        if size >= MMAP_THRESHOLD {
            return alloc_mmap(size);
        }

        init_heap();
        if (*HEAP_START.get()).0.is_null() {
            return ptr::null_mut();
        }

        let aligned_size = align_up_usize(size, ALIGNMENT).max(MIN_BLOCK_SIZE);
        let mut block = (*FREE_LIST.get()).0.find_first_fit(aligned_size);

        if block.is_null() {
            block = extend_heap(aligned_size);
            if block.is_null() {
                return ptr::null_mut();
            }
        }

        (*FREE_LIST.get()).0.remove(block);

        let split_block = try_split_block(block, aligned_size, MIN_BLOCK_SIZE);
        if !split_block.is_null() {
            (*FREE_LIST.get()).0.push_front(split_block);
        }

        (*block).mark_allocated();
        BlockHeader::data_ptr(block) as *mut c_void
    }
}

pub fn dealloc(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }

    unsafe {
        let block = BlockHeader::from_data_ptr(ptr as *mut u8);

        if !(*block).is_valid() || !(*block).is_allocated() {
            return;
        }

        if (*block).flags & MMAP_FLAG != 0 {
            let total = (*block).total_size();
            syscall2(SYSCALL_MUNMAP, block as u64, total as u64);
            return;
        }

        (*block).mark_free();
        (*FREE_LIST.get()).0.push_front(block);
        try_coalesce_forward(block);
    }
}

pub fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    if ptr.is_null() {
        return alloc(size);
    }

    if size == 0 {
        dealloc(ptr);
        return ptr::null_mut();
    }

    unsafe {
        let block = BlockHeader::from_data_ptr(ptr as *mut u8);

        if !(*block).is_valid() {
            return ptr::null_mut();
        }

        let old_size = (*block).size as usize;
        let aligned_size = align_up_usize(size, ALIGNMENT).max(MIN_BLOCK_SIZE);

        if old_size >= aligned_size && (*block).flags & MMAP_FLAG == 0 {
            return ptr;
        }

        let new_ptr = alloc(size);
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        let copy_size = if old_size < size { old_size } else { size };
        ptr::copy_nonoverlapping(ptr as *const u8, new_ptr as *mut u8, copy_size);
        dealloc(ptr);

        new_ptr
    }
}

pub fn calloc(nmemb: usize, size: usize) -> *mut c_void {
    let total = match nmemb.checked_mul(size) {
        Some(t) => t,
        None => return ptr::null_mut(),
    };

    let ptr = alloc(total);
    if !ptr.is_null() {
        unsafe {
            ptr::write_bytes(ptr as *mut u8, 0, total);
        }
    }
    ptr
}

/// Allocate memory with a specific alignment.
///
/// For alignments <= 16, delegates to the standard allocator.
/// For larger alignments, uses mmap (page-aligned = 4096-aligned).
pub fn memalign(alignment: usize, size: usize) -> *mut u8 {
    if size == 0 {
        return ptr::null_mut();
    }

    if alignment <= ALIGNMENT {
        return alloc(size) as *mut u8;
    }

    unsafe {
        if alignment <= PAGE_SIZE {
            return alloc_mmap(size) as *mut u8;
        }

        let total = size + alignment + HEADER_SIZE;
        let mapped = align_up_usize(total, PAGE_SIZE);
        let ret = syscall6(
            SYSCALL_MMAP,
            0,
            mapped as u64,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            (-1i64) as u64,
            0,
        );
        match crate::demux(ret) {
            Ok(addr) => {
                let base = addr as usize;
                let data_start = base + HEADER_SIZE;
                let aligned_data = align_up_usize(data_start, alignment);
                let block = (aligned_data - HEADER_SIZE) as *mut BlockHeader;
                BlockHeader::init(
                    block,
                    (mapped - (aligned_data - base)) as u32,
                    MAGIC_ALLOCATED,
                );
                (*block).flags = MMAP_FLAG;
                (*block).update_checksum();
                aligned_data as *mut u8
            }
            Err(_) => ptr::null_mut(),
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memalign_ffi(alignment: usize, size: usize) -> *mut u8 {
    memalign(alignment, size)
}

pub fn malloc_usable_size(ptr: *mut u8) -> usize {
    if ptr.is_null() {
        return 0;
    }
    unsafe {
        let block = BlockHeader::from_data_ptr(ptr);
        if !(*block).is_valid() || !(*block).is_allocated() {
            return 0;
        }
        (*block).size as usize
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc_usable_size_ffi(ptr: *mut u8) -> usize {
    malloc_usable_size(ptr)
}
