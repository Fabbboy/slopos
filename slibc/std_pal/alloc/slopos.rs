use super::realloc_fallback;
use crate::alloc::Layout;

// Bound under c_* names: the public `realloc` below would otherwise shadow the
// extern and recurse into itself.
unsafe extern "C" {
    #[link_name = "malloc"]
    fn c_malloc(size: usize) -> *mut u8;
    #[link_name = "free"]
    fn c_free(ptr: *mut u8);
    #[link_name = "realloc"]
    fn c_realloc(ptr: *mut u8, size: usize) -> *mut u8;
    #[link_name = "calloc"]
    fn c_calloc(nmemb: usize, size: usize) -> *mut u8;
    #[link_name = "memalign_ffi"]
    fn c_memalign(alignment: usize, size: usize) -> *mut u8;
}

const MALLOC_ALIGN: usize = 16;

#[inline]
pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    if layout.align() <= MALLOC_ALIGN {
        unsafe { c_malloc(layout.size()) }
    } else {
        unsafe { c_memalign(layout.align(), layout.size()) }
    }
}

#[inline]
pub unsafe fn dealloc(ptr: *mut u8, _layout: Layout) {
    unsafe { c_free(ptr) }
}

#[inline]
pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    if layout.align() <= MALLOC_ALIGN {
        unsafe { c_calloc(1, layout.size()) }
    } else {
        let ptr = unsafe { alloc(layout) };
        if !ptr.is_null() {
            unsafe { ptr.write_bytes(0, layout.size()) };
        }
        ptr
    }
}

#[inline]
pub unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    if layout.align() <= MALLOC_ALIGN {
        unsafe { c_realloc(ptr, new_size) }
    } else {
        unsafe { realloc_fallback(ptr, layout, new_size) }
    }
}
