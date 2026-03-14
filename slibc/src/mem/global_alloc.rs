use core::alloc::{GlobalAlloc, Layout};

pub struct SlibcAllocator;

unsafe impl GlobalAlloc for SlibcAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() <= super::malloc::ALIGNMENT {
            super::malloc::alloc(layout.size()) as *mut u8
        } else {
            super::malloc::memalign(layout.align(), layout.size())
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        super::malloc::dealloc(ptr as *mut core::ffi::c_void);
    }

    unsafe fn realloc(&self, ptr: *mut u8, _old_layout: Layout, new_size: usize) -> *mut u8 {
        super::malloc::realloc(ptr as *mut core::ffi::c_void, new_size) as *mut u8
    }
}

#[global_allocator]
static ALLOCATOR: SlibcAllocator = SlibcAllocator;
