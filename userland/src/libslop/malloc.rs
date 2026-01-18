#![allow(unsafe_op_in_unsafe_fn)]
#![allow(static_mut_refs)]

use core::ffi::c_void;
use core::ptr;

use slopos_lib::align_up_usize;
use slopos_lib::free_list::{
    BlockHeader, FreeList, HEADER_SIZE, MAGIC_FREE, MIN_BLOCK_SIZE, try_split_block,
};

use super::syscall::sys_brk;

const ALIGNMENT: usize = 16;
const INITIAL_HEAP_SIZE: usize = 64 * 1024;
const EXTEND_MIN_SIZE: usize = 64 * 1024;

static mut HEAP_START: *mut BlockHeader = ptr::null_mut();
static mut HEAP_END: *mut u8 = ptr::null_mut();
static mut FREE_LIST: FreeList = FreeList::new();

unsafe fn init_heap() {
    if !HEAP_START.is_null() {
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

    HEAP_START = current_brk as *mut BlockHeader;
    HEAP_END = new_brk;

    let first_block = HEAP_START;
    BlockHeader::init(
        first_block,
        (INITIAL_HEAP_SIZE - HEADER_SIZE) as u32,
        MAGIC_FREE,
    );
    FREE_LIST.push_front(first_block);
}

unsafe fn extend_heap(min_size: usize) -> *mut BlockHeader {
    let extend_size = align_up_usize(min_size + HEADER_SIZE, ALIGNMENT).max(EXTEND_MIN_SIZE);
    let new_brk = HEAP_END.add(extend_size);
    let result = sys_brk(new_brk as *mut c_void) as *mut u8;

    if result != new_brk {
        return ptr::null_mut();
    }

    let new_block = HEAP_END as *mut BlockHeader;
    BlockHeader::init(new_block, (extend_size - HEADER_SIZE) as u32, MAGIC_FREE);
    FREE_LIST.push_front(new_block);
    HEAP_END = new_brk;

    new_block
}

unsafe fn try_coalesce_forward(block: *mut BlockHeader) {
    let block_end = BlockHeader::block_end(block);
    if block_end >= HEAP_END {
        return;
    }

    let next = block_end as *mut BlockHeader;
    if !(*next).is_valid() || !(*next).is_free() {
        return;
    }

    FREE_LIST.remove(next);
    (*block).size += HEADER_SIZE as u32 + (*next).size;
    (*block).update_checksum();
}

pub fn alloc(size: usize) -> *mut c_void {
    if size == 0 {
        return ptr::null_mut();
    }

    unsafe {
        init_heap();
        if HEAP_START.is_null() {
            return ptr::null_mut();
        }

        let aligned_size = align_up_usize(size, ALIGNMENT).max(MIN_BLOCK_SIZE);
        let mut block = FREE_LIST.find_first_fit(aligned_size);

        if block.is_null() {
            block = extend_heap(aligned_size);
            if block.is_null() {
                return ptr::null_mut();
            }
        }

        FREE_LIST.remove(block);

        let split_block = try_split_block(block, aligned_size, MIN_BLOCK_SIZE);
        if !split_block.is_null() {
            FREE_LIST.push_front(split_block);
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

        (*block).mark_free();
        FREE_LIST.push_front(block);
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

        if old_size >= aligned_size {
            return ptr;
        }

        let new_ptr = alloc(size);
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        ptr::copy_nonoverlapping(ptr as *const u8, new_ptr as *mut u8, old_size);
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
