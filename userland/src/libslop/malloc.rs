#![allow(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use core::ptr;

use slopos_lib::align_up_usize;

use super::syscall::sys_brk;

const BLOCK_MAGIC: u32 = 0xDEAD_BEEF;
const MIN_ALLOC_SIZE: usize = 16;
const ALIGNMENT: usize = 16;

#[repr(C)]
struct BlockHeader {
    magic: u32,
    size: u32,
    is_free: u32,
    _padding: u32,
    next: *mut BlockHeader,
    prev: *mut BlockHeader,
}

const HEADER_SIZE: usize = core::mem::size_of::<BlockHeader>();

static mut HEAP_START: *mut BlockHeader = ptr::null_mut();
static mut HEAP_END: *mut u8 = ptr::null_mut();
static mut FREE_LIST: *mut BlockHeader = ptr::null_mut();

unsafe fn init_heap() {
    if !HEAP_START.is_null() {
        return;
    }

    let current_brk = sys_brk(ptr::null_mut()) as *mut u8;
    if current_brk.is_null() || current_brk as usize == usize::MAX {
        return;
    }

    let initial_size: usize = 64 * 1024;
    let new_brk = current_brk.add(initial_size);
    let result = sys_brk(new_brk as *mut c_void) as *mut u8;

    if result != new_brk {
        return;
    }

    HEAP_START = current_brk as *mut BlockHeader;
    HEAP_END = new_brk;

    let first_block = HEAP_START;
    (*first_block).magic = BLOCK_MAGIC;
    (*first_block).size = (initial_size - HEADER_SIZE) as u32;
    (*first_block).is_free = 1;
    (*first_block).next = ptr::null_mut();
    (*first_block).prev = ptr::null_mut();
    FREE_LIST = first_block;
}

unsafe fn extend_heap(min_size: usize) -> *mut BlockHeader {
    let extend_size = align_up_usize(min_size + HEADER_SIZE, ALIGNMENT).max(64 * 1024);
    let new_brk = HEAP_END.add(extend_size);
    let result = sys_brk(new_brk as *mut c_void) as *mut u8;

    if result != new_brk {
        return ptr::null_mut();
    }

    let new_block = HEAP_END as *mut BlockHeader;
    (*new_block).magic = BLOCK_MAGIC;
    (*new_block).size = (extend_size - HEADER_SIZE) as u32;
    (*new_block).is_free = 1;
    (*new_block).next = FREE_LIST;
    (*new_block).prev = ptr::null_mut();

    if !FREE_LIST.is_null() {
        (*FREE_LIST).prev = new_block;
    }
    FREE_LIST = new_block;
    HEAP_END = new_brk;

    new_block
}

unsafe fn find_free_block(size: usize) -> *mut BlockHeader {
    let mut current = FREE_LIST;
    while !current.is_null() {
        if (*current).is_free != 0 && (*current).size as usize >= size {
            return current;
        }
        current = (*current).next;
    }
    ptr::null_mut()
}

unsafe fn split_block(block: *mut BlockHeader, size: usize) {
    let block_size = (*block).size as usize;
    let remaining = block_size - size;

    if remaining >= HEADER_SIZE + MIN_ALLOC_SIZE {
        let new_block = (block as *mut u8).add(HEADER_SIZE + size) as *mut BlockHeader;
        (*new_block).magic = BLOCK_MAGIC;
        (*new_block).size = (remaining - HEADER_SIZE) as u32;
        (*new_block).is_free = 1;
        (*new_block).next = (*block).next;
        (*new_block).prev = block;

        if !(*block).next.is_null() {
            (*(*block).next).prev = new_block;
        }
        (*block).next = new_block;
        (*block).size = size as u32;
    }
}

unsafe fn remove_from_free_list(block: *mut BlockHeader) {
    if !(*block).prev.is_null() {
        (*(*block).prev).next = (*block).next;
    } else {
        FREE_LIST = (*block).next;
    }
    if !(*block).next.is_null() {
        (*(*block).next).prev = (*block).prev;
    }
}

unsafe fn add_to_free_list(block: *mut BlockHeader) {
    (*block).next = FREE_LIST;
    (*block).prev = ptr::null_mut();
    if !FREE_LIST.is_null() {
        (*FREE_LIST).prev = block;
    }
    FREE_LIST = block;
}

unsafe fn coalesce(block: *mut BlockHeader) {
    let block_end = (block as *mut u8).add(HEADER_SIZE + (*block).size as usize);

    if !(*block).next.is_null() {
        let next = (*block).next;
        let next_start = next as *mut u8;
        if block_end == next_start && (*next).is_free != 0 && (*next).magic == BLOCK_MAGIC {
            (*block).size += HEADER_SIZE as u32 + (*next).size;
            (*block).next = (*next).next;
            if !(*next).next.is_null() {
                (*(*next).next).prev = block;
            }
        }
    }
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

        let aligned_size = align_up_usize(size, ALIGNMENT).max(MIN_ALLOC_SIZE);
        let mut block = find_free_block(aligned_size);

        if block.is_null() {
            block = extend_heap(aligned_size);
            if block.is_null() {
                return ptr::null_mut();
            }
        }

        split_block(block, aligned_size);
        (*block).is_free = 0;
        remove_from_free_list(block);

        (block as *mut u8).add(HEADER_SIZE) as *mut c_void
    }
}

pub fn dealloc(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }

    unsafe {
        let block = (ptr as *mut u8).sub(HEADER_SIZE) as *mut BlockHeader;

        if (*block).magic != BLOCK_MAGIC {
            return;
        }

        if (*block).is_free != 0 {
            return;
        }

        (*block).is_free = 1;
        add_to_free_list(block);
        coalesce(block);
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
        let block = (ptr as *mut u8).sub(HEADER_SIZE) as *mut BlockHeader;

        if (*block).magic != BLOCK_MAGIC {
            return ptr::null_mut();
        }

        let old_size = (*block).size as usize;
        let aligned_size = align_up_usize(size, ALIGNMENT).max(MIN_ALLOC_SIZE);

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
