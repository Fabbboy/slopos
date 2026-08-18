//! C-ABI-style `kmalloc` / `kfree` / `kzalloc` / `HeapStats` surface.
//!
//! Routes through [`super::KERNEL_SLAB`] rather than the
//! `#[global_allocator]`, so the `mm` crate stays `#![forbid(unsafe_code)]`.

use core::ffi::c_int;
use core::ffi::c_void;
use core::ptr::NonNull;

use slopos_ostd::klog_info;
use slopos_ostd::mm::KernelHeapBackend;
use slopos_ostd::util::ptr_buf;

pub use super::stats::HeapStats;

/// Largest single `kmalloc` request; larger requests return null.
pub use super::MAX_ALLOC_SIZE;

/// Allocate `size` bytes of kernel heap memory, zeroed.
///
/// Returns null on `size == 0`, `size > MAX_ALLOC_SIZE`, or allocation
/// failure.
pub fn kmalloc(size: usize) -> *mut c_void {
    if size == 0 || size > MAX_ALLOC_SIZE {
        return core::ptr::null_mut();
    }
    let Some(ptr) = super::KERNEL_SLAB.alloc(size) else {
        return core::ptr::null_mut();
    };
    // Tail padding past `size` is never read by the caller, so the rounded
    // chunk is left unscrubbed.
    ptr_buf::with_buf_mut(ptr.as_ptr(), size, |bytes: &mut [u8]| bytes.fill(0));
    ptr.as_ptr() as *mut c_void
}

/// Alias for [`kmalloc`], which already zeroes.
pub fn kzalloc(size: usize) -> *mut c_void {
    kmalloc(size)
}

/// Return a previously [`kmalloc`]-ed pointer to the slab/large tier. Null is
/// a no-op; wild pointers are silently swallowed.
pub fn kfree(ptr_in: *mut c_void) {
    let Some(nn) = NonNull::new(ptr_in as *mut u8) else {
        return;
    };
    super::KERNEL_SLAB.dealloc(nn);
}

/// Snapshot of the kernel heap's slab and large-tier counters.
pub fn get_heap_stats_owned() -> HeapStats {
    super::stats::snapshot()
}

/// Legacy C-ABI knob, now a no-op: [`super::allocator`] runs its
/// poison/redzone checks unconditionally.
pub fn kernel_heap_enable_diagnostics(_enable: c_int) {}

pub fn print_heap_stats() {
    let s = super::stats::snapshot();
    klog_info!("=== Kernel Heap Statistics ===");
    klog_info!("Total size: {} bytes", s.total_size);
    klog_info!("Allocated: {} bytes", s.allocated_size);
    klog_info!("Free: {} bytes", s.free_size);
    klog_info!("Allocations: {}", s.allocation_count);
    klog_info!("Frees: {}", s.free_count);
}

/// Re-exported so `mm/src/tests/tests.rs` can assert the warmup minimum
/// without reaching into `super`.
pub use super::HEAP_WARMUP_PAGES;
