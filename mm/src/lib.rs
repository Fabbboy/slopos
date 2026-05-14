#![no_std]
#![feature(sync_unsafe_cell)]
#![forbid(unsafe_code)]

pub mod aslr;
pub mod cow;
pub mod demand;
pub mod dual_paging;
pub mod elf;
pub mod error;
pub mod frame_alloc_shim;
pub mod hhdm;
pub mod io_mem_mapper_shim;
pub mod kernel_heap;
pub mod kernel_mappings;
pub mod kernel_meta;
pub mod memfd;
pub mod memory_init;
pub mod memory_layout;
pub mod memory_layout_defs;
pub mod memory_reservations;
pub mod mmio;
pub mod mmu;
pub mod page_alloc;
pub mod page_fault;
pub mod paging;
pub mod paging_defs;
pub mod pat;
pub mod process_vm;
pub mod stack_region;
pub mod stack_va;
pub mod symbols;
#[cfg(feature = "test-hooks")]
pub mod tests;
pub mod tlb;
pub mod user_copy;
pub mod user_io_buf;
pub mod user_ptr;
pub mod vma_region;

use core::alloc::Layout;
use core::ffi::c_void;
use slopos_ostd::align_up_usize;
use slopos_ostd::mm::heap::register_kernel_heap_backend;
use slopos_ostd::sync::BspToken;

/// Safe-callback adapter for the slab allocator. Invoked by OSTD's
/// `KernelHeap::alloc` after the slab tier is wired up. `align > 16`
/// alignment fixup is OSTD's responsibility — this callback handles the
/// `align <= 16` slab path verbatim by routing through `kmalloc`.
fn slab_alloc_cb(size: usize) -> *mut u8 {
    crate::kernel_heap::kmalloc(size) as *mut u8
}

fn slab_dealloc_cb(ptr: *mut u8) {
    crate::kernel_heap::kfree(ptr as *mut c_void);
}

/// Promote the global allocator from OSTD's bootstrap bump pool to the
/// `mm` crate's slab allocator. Must run after `init_kernel_heap()`
/// completes so the slab backing pages are mapped.
///
/// The `&BspToken<'brand>` witness binds the call to the BSP-init
/// scope opened by `slopos_ostd::sync::run_bsp_init`; it is forwarded
/// to OSTD's [`register_kernel_heap_backend`], which is one-shot.
pub fn global_allocator_use_kernel_heap<'brand>(token: &BspToken<'brand>) {
    let _ = align_up_usize(0, 16); // keep slopos_ostd import alive for layout math users
    let _ = Layout::new::<u8>(); // keep core::alloc::Layout import alive
    register_kernel_heap_backend(token, slab_alloc_cb, slab_dealloc_cb);
}
