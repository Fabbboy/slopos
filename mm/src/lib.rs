#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![feature(sync_unsafe_cell)]

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

use core::alloc::{GlobalAlloc, Layout};
use core::cell::SyncUnsafeCell;
use core::mem;
use core::ptr;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use slopos_utils::{align_up, align_up_usize};

const HEAP_SIZE: usize = 2 * 1024 * 1024;

/// Aligned heap storage wrapper.
/// The HEAP must be properly aligned (at least 16 bytes) so that allocations
/// requesting alignment up to 16 bytes will get properly aligned pointers.
/// Without this, the base address of a [u8; N] array has alignment 1, causing
/// unaligned pointer panics in collections like VecDeque.
#[repr(C, align(16))]
struct AlignedHeap([u8; HEAP_SIZE]);

#[unsafe(link_section = ".bss.heap")]
static HEAP: SyncUnsafeCell<AlignedHeap> = SyncUnsafeCell::new(AlignedHeap([0; HEAP_SIZE]));

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
        unsafe { (*HEAP.get()).0.as_mut_ptr().add(offset) }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // The bump allocator never frees; this is acceptable for early kernel bring-up.
    }
}

const ALLOC_MODE_BUMP: u8 = 0;
const ALLOC_MODE_SLAB: u8 = 1;
static GLOBAL_ALLOC_MODE: AtomicU8 = AtomicU8::new(ALLOC_MODE_BUMP);
static GLOBAL_BUMP_ALLOCATOR: BumpAllocator = BumpAllocator::new();

/// Global allocator entry point invoked by `slopos-ostd`'s
/// `KernelHeap` shim. The two `slopos_global_*` symbols are the
/// extern-Rust contract between OSTD and mm: OSTD declares them in
/// `slopos-ostd::mm::heap` as `unsafe extern "Rust" fn`, mm defines
/// them here with matching signatures.
#[unsafe(no_mangle)]
unsafe extern "Rust" fn slopos_global_alloc(layout: Layout) -> *mut u8 {
    if GLOBAL_ALLOC_MODE.load(Ordering::Acquire) == ALLOC_MODE_SLAB {
        let align = layout.align().max(16);
        let size = layout.size();
        if align <= 16 {
            return crate::kernel_heap::kmalloc(size) as *mut u8;
        }

        let extra = align_up_usize(mem::size_of::<usize>(), 16);
        let total = size.saturating_add(align).saturating_add(extra);
        let raw = crate::kernel_heap::kmalloc(total) as *mut u8;
        if raw.is_null() {
            return ptr::null_mut();
        }

        let base = raw as usize;
        let aligned = align_up_usize(base.saturating_add(extra), align);
        let slot = (aligned - mem::size_of::<usize>()) as *mut usize;
        unsafe {
            *slot = base;
        }
        return aligned as *mut u8;
    }

    unsafe { GLOBAL_BUMP_ALLOCATOR.alloc(layout) }
}

#[unsafe(no_mangle)]
unsafe extern "Rust" fn slopos_global_dealloc(ptr: *mut u8, layout: Layout) {
    if ptr.is_null() {
        return;
    }

    if GLOBAL_ALLOC_MODE.load(Ordering::Acquire) == ALLOC_MODE_SLAB {
        let align = layout.align().max(16);
        if align <= 16 {
            crate::kernel_heap::kfree(ptr as *mut _);
            return;
        }

        let slot = (ptr as usize).saturating_sub(mem::size_of::<usize>()) as *mut usize;
        let raw = unsafe { *slot } as *mut u8;
        if !raw.is_null() {
            crate::kernel_heap::kfree(raw as *mut _);
        }
        return;
    }

    unsafe { GLOBAL_BUMP_ALLOCATOR.dealloc(ptr, layout) }
}

pub fn global_allocator_use_kernel_heap() {
    GLOBAL_ALLOC_MODE.store(ALLOC_MODE_SLAB, Ordering::Release);
}
