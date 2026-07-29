//! C-ABI-style `kmalloc` / `kfree` / `kzalloc` / `HeapStats` surface.
//!
//! Routes directly through [`super::KERNEL_SLAB`] (not through the
//! `#[global_allocator]`) so the `mm` crate stays
//! `#![forbid(unsafe_code)]` — calling `alloc::alloc::*` would
//! require an `unsafe` block here. Pointers in/out; safe-Rust slab
//! dispatch underneath.

use core::ffi::c_int;
use core::ffi::c_void;
use core::ptr::NonNull;

use slopos_ostd::klog_info;
use slopos_ostd::mm::KernelHeapBackend;
use slopos_ostd::util::ptr_buf;

pub use super::stats::HeapStats;

/// Largest single `kmalloc` request (1 MiB). Allocations larger than
/// this fail (return null).
pub use super::MAX_ALLOC_SIZE;

/// Allocate `size` bytes of kernel heap memory, zeroed.
///
/// Returns null on `size == 0`, `size > MAX_ALLOC_SIZE`, or
/// allocation failure. The bytes returned are always zero-initialised
/// — callers that received recycled `(i & 0xFF)`-pattern bytes from
/// previous owners decoded them as control-flow data on freed objects
/// (the canonical `0xdfdedddcdbdad9d8`-shape wild-RIP bug), so the
/// safe public surface scrubs unconditionally.
pub fn kmalloc(size: usize) -> *mut c_void {
    if size == 0 || size > MAX_ALLOC_SIZE {
        return core::ptr::null_mut();
    }
    let Some(ptr) = super::KERNEL_SLAB.alloc(size) else {
        return core::ptr::null_mut();
    };
    // Zero exactly the requested size. Tail padding past `size` is
    // never read by the caller, so we don't scrub the rounded chunk.
    ptr_buf::with_buf_mut(ptr.as_ptr(), size, |bytes: &mut [u8]| bytes.fill(0));
    ptr.as_ptr() as *mut c_void
}

/// Transparent alias for [`kmalloc`] — `kmalloc` zeroes by default;
/// kept for source compatibility with callers that still distinguish
/// zero vs. uninitialised allocations.
pub fn kzalloc(size: usize) -> *mut c_void {
    kmalloc(size)
}

/// Return a previously [`kmalloc`]-ed pointer to the slab/large tier.
/// Safe to call on `null` (no-op). Wild pointers are silently
/// swallowed.
pub fn kfree(ptr_in: *mut c_void) {
    let Some(nn) = NonNull::new(ptr_in as *mut u8) else {
        return;
    };
    super::KERNEL_SLAB.dealloc(nn);
}

/// Write a `HeapStats` snapshot to a C-ABI output slot if non-null.
pub fn get_heap_stats(stats: *mut HeapStats) {
    ptr_buf::nullable_write(stats, super::stats::snapshot());
}

/// Owned-return variant of [`get_heap_stats`] for safe-fn callers.
pub fn get_heap_stats_owned() -> HeapStats {
    super::stats::snapshot()
}

/// Legacy C-ABI knob for runtime diagnostic toggling. Currently a
/// no-op — poison/redzone checks now run unconditionally in
/// [`super::allocator`].
pub fn kernel_heap_enable_diagnostics(_enable: c_int) {}

/// Print a human-readable heap snapshot via `klog_info!`.
pub fn print_heap_stats() {
    let s = super::stats::snapshot();
    klog_info!("=== Kernel Heap Statistics ===");
    klog_info!("Total size: {} bytes", s.total_size);
    klog_info!("Allocated: {} bytes", s.allocated_size);
    klog_info!("Free: {} bytes", s.free_size);
    klog_info!("Allocations: {}", s.allocation_count);
    klog_info!("Frees: {}", s.free_count);
}

/// Re-export of the minimum kernel-mapping-warmup page count used by
/// [`super::warmup_for_soft_reboot`]. Exposed here so the test in
/// `mm/src/tests/tests.rs` can assert the minimum without depending
/// on `super::HEAP_WARMUP_PAGES` directly.
pub use super::HEAP_WARMUP_PAGES;
