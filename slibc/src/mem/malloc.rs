use core::ffi::c_void;
use core::ptr;

use super::chunk;
use super::dlmalloc::ALLOCATOR;

pub const ALIGNMENT: usize = chunk::ALIGNMENT;

#[derive(Clone, Copy, Debug)]
pub struct HeapStats {
    pub arena_size: usize,   // total address space in arena segments
    pub largest_free: usize, // largest binned free chunk
    pub direct_count: usize, // live direct (whole-mapping) allocations
}

pub fn alloc(size: usize) -> *mut c_void {
    ALLOCATOR.lock().alloc(size)
}

pub fn dealloc(ptr: *mut c_void) {
    ALLOCATOR.lock().dealloc(ptr)
}

pub fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    ALLOCATOR.lock().realloc(ptr, size)
}

pub fn calloc(nmemb: usize, size: usize) -> *mut c_void {
    let total = match nmemb.checked_mul(size) {
        Some(t) => t,
        None => return ptr::null_mut(),
    };

    let ptr = ALLOCATOR.lock().alloc(total);
    if !ptr.is_null() {
        unsafe {
            ptr::write_bytes(ptr as *mut u8, 0, total);
        }
    }
    ptr
}

pub fn memalign(alignment: usize, size: usize) -> *mut u8 {
    ALLOCATOR.lock().memalign(alignment, size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memalign_ffi(alignment: usize, size: usize) -> *mut u8 {
    memalign(alignment, size)
}

pub fn malloc_usable_size(ptr: *mut u8) -> usize {
    ALLOCATOR.lock().malloc_usable_size(ptr)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc_usable_size_ffi(ptr: *mut u8) -> usize {
    malloc_usable_size(ptr)
}

pub fn heap_stats() -> HeapStats {
    let guard = ALLOCATOR.lock();

    HeapStats {
        arena_size: guard.arena_size(),
        largest_free: guard.largest_free_chunk(),
        direct_count: guard.direct_region_count(),
    }
}
