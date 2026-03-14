use crate::alloc::{GlobalAlloc, Layout, System};

unsafe extern "C" {
    fn malloc(size: usize) -> *mut u8;
    fn free(ptr: *mut u8);
    fn realloc(ptr: *mut u8, size: usize) -> *mut u8;
    fn calloc(nmemb: usize, size: usize) -> *mut u8;
    fn memalign_ffi(alignment: usize, size: usize) -> *mut u8;
}

#[stable(feature = "alloc_system_type", since = "1.28.0")]
unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() <= 16 {
            unsafe { malloc(layout.size()) }
        } else {
            unsafe { memalign_ffi(layout.align(), layout.size()) }
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        unsafe { free(ptr) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.align() <= 16 {
            unsafe { calloc(1, layout.size()) }
        } else {
            let ptr = unsafe { self.alloc(layout) };
            if !ptr.is_null() {
                unsafe {
                    core::ptr::write_bytes(ptr, 0, layout.size());
                }
            }
            ptr
        }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.align() <= 16 {
            unsafe { realloc(ptr, new_size) }
        } else {
            unsafe { super::realloc_fallback(self, ptr, layout, new_size) }
        }
    }
}
