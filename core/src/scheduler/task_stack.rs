//! Generic RAII handle owning one task stack: VA slot + physical frames.
//!
//! One implementation parameterised over [`StackRegion`].  Replaces the
//! historical `stack.rs` (kernel stacks) and `unsafe_stack.rs`
//! (SafeStack data stacks) mirror modules.
//!
//! # Layout (stack of `size` bytes, stride `R::STRIDE`)
//!
//! ```text
//!  ┌──────────────────────────┐ ← top()  (slot_base + R::STRIDE on a
//!  │  usable stack (mapped)   │           default 32 KB / 8 KB slot)
//!  │       size bytes         │
//!  ├──────────────────────────┤ ← base() = slot_base + R::GUARD_SIZE
//!  │  guard page (unmapped)   │
//!  └──────────────────────────┘ ← slot_base
//! ```
//!
//! Stack pointers start at `top()` and grow downward.  An overflow past
//! `base()` hits the unmapped guard page, producing a clean page fault.
//!
//! # Drop semantics — intentionally do NOT unmap
//!
//! Dropping a `TaskStack<R>` returns the VA slot to the per-CPU cache
//! with the physical-frame mapping still installed.  Unmapping a
//! kernel-VA page would broadcast a TLB shootdown IPI per page; under
//! task churn (tests that create and destroy thousands of tasks) the
//! shootdown path becomes the bottleneck and can hang CPUs.
//!
//! When the slot is later reused, [`StackSlot::was_backed`] is `true`
//! and the allocate path skips frame allocation + page mapping
//! entirely — it just re-zeros the stack contents.
//!
//! Peak physical memory = peak concurrent tasks × stack size, summed
//! across regions.
//!
//! # Safety
//!
//! The implementation is **safe Rust**.  `map_page_4kb` and
//! `unmap_page` are safe wrappers in `mm::paging`, so no `unsafe`
//! blocks are needed here.  Correctness comes from:
//!
//! - Exclusive ownership of the slot (via [`StackSlot<R>`], which is
//!   neither `Copy` nor `Clone`) — no two `TaskStack`s can ever name
//!   the same VA range.
//! - `VirtAddr::new` enforces canonical form.
//! - The guard page is always included by the stride arithmetic; it
//!   cannot be accidentally mapped.

use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_mm::page_alloc::{alloc_page_frames_pcp_batch, free_page_frame};
use slopos_mm::paging::{map_page_4kb, unmap_page};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::stack_region::StackRegion;
use slopos_mm::stack_va::{self, StackSlot};

/// Maximum pages that fit in any region's slot (excluding the guard
/// page).  Sized to cover the largest stride any current or near-future
/// region uses; both kstack and ustack today use a 64 KB stride →
/// 16 pages.  Bump this if a new region needs more.  The compile-time
/// assertion in `TaskStack::<R>::allocate` rejects regions whose stride
/// would overflow this buffer.
const MAX_STACK_PAGES_PER_SLOT: usize = 16;

/// Reasons `TaskStack::<R>::allocate` can fail.  Every variant maps
/// directly to a recoverable resource exhaustion or a caller error;
/// the kernel handles each by returning the error to the task creator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum StackAllocError {
    /// `size` was zero, not a multiple of 4 KB, or larger than
    /// `R::STRIDE - R::GUARD_SIZE`.
    InvalidSize,
    /// No free slot remains in the `R` VA region.
    OutOfVirtualSpace,
    /// The page allocator could not satisfy a frame request.
    OutOfPhysicalFrames,
    /// `map_page_4kb` returned an error (typically out of memory for
    /// intermediate page tables).
    MappingFailed,
}

/// Owning handle to one mapped task stack in region `R`.
///
/// Not `Copy`/`Clone` — double-free is impossible by construction.
#[allow(dead_code)]
pub struct TaskStack<R: StackRegion> {
    /// RAII handle to the VA slot.  Dropped after the empty `Drop`
    /// body of `TaskStack` so its return-to-cache logic runs at the
    /// right point in the destruction order.
    slot: StackSlot<R>,
    /// Number of bytes mapped (excluding the guard page).
    size: u32,
}

#[allow(dead_code)]
impl<R: StackRegion> TaskStack<R> {
    /// Compile-time check: `R`'s stride must fit in our worst-case
    /// frames buffer.  Triggered by referencing `_FITS` from `allocate`.
    const _FITS: () = assert!(
        (R::STRIDE / PAGE_SIZE_4KB) as usize <= MAX_STACK_PAGES_PER_SLOT,
        "TaskStack: region stride exceeds MAX_STACK_PAGES_PER_SLOT — bump the constant",
    );

    /// Allocate a stack of `size` bytes in region `R`.
    ///
    /// `size` must be:
    /// - nonzero,
    /// - a multiple of `PAGE_SIZE_4KB` (4 KB),
    /// - small enough that `size + R::GUARD_SIZE <= R::STRIDE`.
    ///
    /// On failure, no resources leak: any partially-mapped pages are
    /// unmapped and their frames freed, and the slot handle is dropped.
    pub fn allocate(size: usize) -> Result<Self, StackAllocError> {
        // Force the compile-time stride check to instantiate per region.
        let _: () = Self::_FITS;

        if size == 0 || size % PAGE_SIZE_4KB as usize != 0 {
            return Err(StackAllocError::InvalidSize);
        }
        if (size as u64) + R::GUARD_SIZE > R::STRIDE {
            return Err(StackAllocError::InvalidSize);
        }

        let mut slot = stack_va::alloc_slot::<R>().ok_or(StackAllocError::OutOfVirtualSpace)?;
        let base = slot.va_base().as_u64() + R::GUARD_SIZE;
        let page_count = size / PAGE_SIZE_4KB as usize;

        if slot.was_backed() {
            // Slot was previously allocated and is still mapped.  Just
            // zero the stack contents for hygiene — no TLB traffic.
            Self::zero_stack_pages(base, page_count);
            return Ok(Self {
                slot,
                size: size as u32,
            });
        }

        // First time this slot is used: allocate frames and map.
        debug_assert!(page_count <= MAX_STACK_PAGES_PER_SLOT);
        let mut frames = [PhysAddr::NULL; MAX_STACK_PAGES_PER_SLOT];
        let got = alloc_page_frames_pcp_batch(&mut frames[..page_count]);
        if got < page_count {
            for j in 0..got {
                free_page_frame(frames[j]);
            }
            return Err(StackAllocError::OutOfPhysicalFrames);
        }

        for i in 0..page_count {
            let va = VirtAddr::new(base + (i as u64) * PAGE_SIZE_4KB);
            let map_rc = map_page_4kb(va, frames[i], R::PAGE_FLAGS);
            if map_rc != 0 {
                // Free the frames we have not mapped yet.
                for j in i..page_count {
                    free_page_frame(frames[j]);
                }
                // Unmap + free anything already installed.
                Self::cleanup_partial(&slot, i);
                return Err(StackAllocError::MappingFailed);
            }
        }

        // Zero the fresh mapping.
        Self::zero_stack_pages(base, page_count);

        // Record that the slot is now backed so future allocs of it
        // reuse the mapping.
        slot.mark_backed();

        Ok(Self {
            slot,
            size: size as u32,
        })
    }

    /// Lowest usable address (inclusive).  Guard page sits below this.
    #[inline]
    pub fn base(&self) -> VirtAddr {
        VirtAddr::new(self.slot.va_base().as_u64() + R::GUARD_SIZE)
    }

    /// One-past-the-top address (exclusive).  Stacks grow downward
    /// from here toward `base()`.
    #[inline]
    pub fn top(&self) -> VirtAddr {
        VirtAddr::new(self.base().as_u64() + self.size as u64)
    }

    /// Usable size in bytes (excluding the guard page).
    #[inline]
    pub fn size(&self) -> usize {
        self.size as usize
    }

    /// Zero `page_count` pages starting at `base`.  Safe because `base`
    /// points into the caller's exclusively-owned stack slot.
    fn zero_stack_pages(base: u64, page_count: usize) {
        let bytes = page_count * PAGE_SIZE_4KB as usize;
        // SAFETY: `base` is the start of a slot-owned mapped region of
        // exactly `bytes` bytes (verified by caller), and we hold
        // exclusive access via the slot handle.
        unsafe {
            ptr::write_bytes(base as *mut u8, 0, bytes);
        }
    }

    /// Unmap & free the first `mapped` pages of `slot`.  Used by
    /// `allocate` on the error path (the slot's own Drop handles VA
    /// release).
    fn cleanup_partial(slot: &StackSlot<R>, mapped: usize) {
        let base = slot.va_base().as_u64() + R::GUARD_SIZE;
        for i in 0..mapped {
            let va = VirtAddr::new(base + (i as u64) * PAGE_SIZE_4KB);
            let pa: PhysAddr = unmap_page(va);
            if !pa.is_null() {
                free_page_frame(pa);
            }
        }
    }
}

impl<R: StackRegion> Drop for TaskStack<R> {
    fn drop(&mut self) {
        // Intentionally do NOT unmap or free frames here.  Unmapping a
        // kernel-VA page triggers a broadcast TLB shootdown; under task
        // churn that floods the IPI path.  We keep the mapping alive
        // and let the next allocation reuse this slot — see
        // `TaskStack::allocate` and `stack_va::StackSlot`.  The slot
        // handle drops next, returning to the per-CPU cache (or
        // spilling to the global if the cache is full).
    }
}
