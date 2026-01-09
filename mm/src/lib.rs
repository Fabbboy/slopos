#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(static_mut_refs)]

pub mod kernel_heap;
pub mod memory_init;
mod memory_layout;
mod memory_reservations;
pub mod mm_constants;
pub mod page_alloc;
pub mod paging;
pub mod phys_virt;
pub mod process_vm;
pub mod shared_memory;
mod tests;
pub mod user_copy;
pub mod user_copy_helpers;
pub mod symbols;

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use slopos_lib::align_up;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

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

pub fn init(hhdm_offset: u64) {
    HHDM_OFFSET.store(hhdm_offset, Ordering::Release);
}

pub fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Acquire)
}

/// Raw HHDM translation: physical to virtual.
/// No safety checks - use mm_phys_to_virt() for checked translation.
#[inline(always)]
pub fn hhdm_phys_to_virt(phys: u64) -> u64 {
    phys + hhdm_offset()
}

/// Raw HHDM translation: virtual to physical.
/// No safety checks - use mm_virt_to_phys() for checked translation.
#[inline(always)]
pub fn hhdm_virt_to_phys(virt: u64) -> u64 {
    virt.wrapping_sub(hhdm_offset())
}

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
