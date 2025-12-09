#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

mod memory_reservations;
mod phys_virt;

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use slopos_lib::align_up;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

const HEAP_SIZE: usize = 2 * 1024 * 1024;

#[link_section = ".bss.heap"]
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

pub fn init(hhdm_offset: u64) {
    HHDM_OFFSET.store(hhdm_offset, Ordering::Release);
}

pub fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Acquire)
}

pub fn phys_to_virt(phys: u64) -> u64 {
    phys + hhdm_offset()
}

pub fn virt_to_phys(virt: u64) -> u64 {
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
        HEAP.as_mut_ptr().add(offset)
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // The bump allocator never frees; this is acceptable for early kernel bring-up.
    }
}

