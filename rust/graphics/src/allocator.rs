//! Global allocator wrapper for kernel memory allocation
//!
//! This provides a GlobalAlloc implementation that uses the kernel's
//! kmalloc/kfree functions, enabling Rust to use Vec, Box, String, etc.

use core::alloc::{GlobalAlloc, Layout};
use bindings as c;

/// Kernel memory allocator
///
/// Wraps kmalloc/kfree to provide Rust's global allocator interface
pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Allocate memory using kernel's kmalloc
        let ptr = c::kmalloc(layout.size());
        ptr as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // Free memory using kernel's kfree
        c::kfree(ptr as *mut core::ffi::c_void);
    }
}

/// Global allocator instance
///
/// This allows Rust to use heap allocation in the kernel
#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;
