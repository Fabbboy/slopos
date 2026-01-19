#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(static_mut_refs)]

pub mod aslr;
pub mod cow;
pub mod demand;
pub mod elf;
pub mod hhdm;
pub mod kernel_heap;
pub mod memory_init;
pub mod memory_layout;
mod memory_reservations;
pub mod mm_constants;
pub mod mmio;
pub mod page_alloc;
pub mod paging;
pub mod process_vm;
pub mod shared_memory;
pub mod symbols;
pub mod tests;
pub mod tests_cow_edge;
pub mod tests_demand;
pub mod tests_oom;
pub mod tlb;
pub mod user_copy;
pub mod user_ptr;
pub mod vma_flags;
pub mod vma_tree;

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};
use slopos_lib::align_up;

const HEAP_SIZE: usize = 2 * 1024 * 1024;

/// Aligned heap storage wrapper.
/// The HEAP must be properly aligned (at least 16 bytes) so that allocations
/// requesting alignment up to 16 bytes will get properly aligned pointers.
/// Without this, the base address of a [u8; N] array has alignment 1, causing
/// unaligned pointer panics in collections like VecDeque.
#[repr(C, align(16))]
struct AlignedHeap([u8; HEAP_SIZE]);

#[unsafe(link_section = ".bss.heap")]
static mut HEAP: AlignedHeap = AlignedHeap([0; HEAP_SIZE]);

pub struct BumpAllocator {
    next: AtomicUsize,
}

impl BumpAllocator {
    pub const fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
        }
    }
}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(8);
        let size = layout.size();
        let mut offset = self.next.load(Ordering::Relaxed);
        offset = align_up(offset, align);
        if offset + size > HEAP_SIZE {
            return ptr::null_mut();
        }
        self.next.store(offset + size, Ordering::Relaxed);
        unsafe { HEAP.0.as_mut_ptr().add(offset) }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // The bump allocator never frees; this is acceptable for early kernel bring-up.
    }
}
