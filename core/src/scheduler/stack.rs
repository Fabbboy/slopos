//! `KernelStack` — RAII handle for a task's kernel-mode stack.
//!
//! Each `KernelStack` owns:
//!
//! 1. A [`KstackSlot`] (VA range in `KSTACK_VA_BASE..KSTACK_VA_END`)
//! 2. One guard page at the bottom of the slot (left unmapped)
//! 3. `size / PAGE_SIZE_4KB` physical frames mapped read/write into the
//!    pages above the guard
//!
//! Dropping the handle zeros the stack pages for hygiene and returns
//! the VA slot to the allocator.  The physical frames and PTE mappings
//! are **kept alive** — dropping does not call `unmap_page`, and
//! therefore does not issue a broadcast TLB shootdown (every kernel-VA
//! unmap fires one IPI per CPU; under task churn that floods the
//! shootdown path and hangs it).
//!
//! When the slot is reused later, `was_backed == true` tells the
//! allocator to skip frame allocation and page mapping entirely — it
//! just re-zeroes the stack.  A per-CPU cache in front of the global
//! slot bitmap keeps the warm path lock-free.  Peak physical memory =
//! peak concurrent tasks × stack size (≤ 8 MB for typical workloads).
//!
//! # Layout (stack of `size` bytes, stride 64 KB)
//!
//! ```text
//!  ┌──────────────────────────┐ ← top()  (slot_base + KSTACK_STRIDE on
//!  │  usable stack (mapped)   │           a default 32 KB / 64 KB slot)
//!  │       size bytes         │
//!  ├──────────────────────────┤ ← base() = slot_base + KSTACK_GUARD_SIZE
//!  │  guard page (unmapped)   │
//!  └──────────────────────────┘ ← slot_base
//! ```
//!
//! Stack pointers start at `top()` and grow downward.  An overflow past
//! `base()` hits the unmapped guard page, producing a clean page fault.
//!
//! # Safety
//!
//! The entire implementation is **safe Rust**.  `map_page_4kb` and
//! `unmap_page` are safe wrappers in `mm::paging`, so we don't need
//! any `unsafe` blocks here.  Correctness comes from:
//!
//! - Exclusive ownership of the slot (via `KstackSlot`, which is
//!   neither `Copy` nor `Clone`) — no two `KernelStack`s can ever name
//!   the same VA range.
//! - `VirtAddr::new` enforces canonical form.
//! - The guard page is always included by the stride arithmetic; it
//!   cannot be accidentally mapped.
//! - RAII drop order: PTEs unmapped → frames freed → VA slot released.

use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_mm::kstack_va::{KstackSlot, alloc_slot};
use slopos_mm::memory_layout_defs::{KSTACK_GUARD_SIZE, KSTACK_STRIDE};
use slopos_mm::page_alloc::{alloc_page_frames_pcp_batch, free_page_frame};
use slopos_mm::paging::{map_page_4kb, unmap_page};
use slopos_mm::paging_defs::{PAGE_SIZE_4KB, PageFlags};

/// Reasons `KernelStack::allocate` can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackAllocError {
    /// `size` was zero, not a multiple of 4 KB, or larger than the slot
    /// stride minus the guard page.
    InvalidSize,
    /// No free slot remains in the kernel-stack VA region.
    OutOfVirtualSpace,
    /// The page allocator couldn't satisfy a 4 KB frame request.
    OutOfPhysicalFrames,
    /// `map_page_4kb` returned an error (typically out of memory for
    /// intermediate page tables).
    MappingFailed,
}

/// Owning handle to a kernel-mode stack backed by the KSTACK VA region.
///
/// Not `Copy`/`Clone` — double-free is impossible by construction.
pub struct KernelStack {
    /// RAII handle to the VA slot.  Dropped last so its cleanup runs
    /// *after* we've unmapped the PTEs.
    slot: KstackSlot,
    /// Number of bytes mapped (excluding the guard page).
    size: u32,
}

impl KernelStack {
    /// Allocate a kernel stack of `size` bytes.
    ///
    /// `size` must be:
    /// - nonzero,
    /// - a multiple of `PAGE_SIZE_4KB` (4 KB),
    /// - small enough that `size + KSTACK_GUARD_SIZE <= KSTACK_STRIDE`.
    ///
    /// On failure, no resources leak: any partially-mapped pages are
    /// unmapped and their frames freed, and the slot handle is dropped.
    pub fn allocate(size: usize) -> Result<Self, StackAllocError> {
        // --- Validate size -------------------------------------------------
        if size == 0 || size % PAGE_SIZE_4KB as usize != 0 {
            return Err(StackAllocError::InvalidSize);
        }
        if (size as u64) + KSTACK_GUARD_SIZE > KSTACK_STRIDE {
            return Err(StackAllocError::InvalidSize);
        }

        // --- Reserve a VA slot --------------------------------------------
        let mut slot = alloc_slot().ok_or(StackAllocError::OutOfVirtualSpace)?;
        let base = slot.va_base().as_u64() + KSTACK_GUARD_SIZE;
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

        // --- First time this slot is used: allocate frames and map -------
        let flags = (PageFlags::KERNEL_RW | PageFlags::NO_EXECUTE).bits();

        // Maximum pages a single slot can hold.
        const MAX_STACK_PAGES: usize = (KSTACK_STRIDE / PAGE_SIZE_4KB) as usize;
        debug_assert!(page_count <= MAX_STACK_PAGES);

        // Batch-allocate every backing frame under one PreemptGuard.
        let mut frames = [PhysAddr::NULL; MAX_STACK_PAGES];
        let got = alloc_page_frames_pcp_batch(&mut frames[..page_count]);
        if got < page_count {
            for j in 0..got {
                free_page_frame(frames[j]);
            }
            return Err(StackAllocError::OutOfPhysicalFrames);
        }

        for i in 0..page_count {
            let va = VirtAddr::new(base + (i as u64) * PAGE_SIZE_4KB);
            let map_rc = map_page_4kb(va, frames[i], flags);
            if map_rc != 0 {
                // Free the frames we haven't mapped yet.
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

    /// Lowest usable address (inclusive).  Guard page sits below this.
    #[inline]
    pub fn base(&self) -> VirtAddr {
        VirtAddr::new(self.slot.va_base().as_u64() + KSTACK_GUARD_SIZE)
    }

    /// One-past-the-top address (exclusive).  Stacks grow downward
    /// from here toward `base()`.
    #[inline]
    pub fn top(&self) -> VirtAddr {
        VirtAddr::new(self.base().as_u64() + self.size as u64)
    }

    /// Usable size in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.size as usize
    }

    /// Unmap & free the first `mapped` pages of this slot.  Used by
    /// `allocate` on error paths and by `Drop` for full cleanup.
    fn cleanup_partial(slot: &KstackSlot, mapped: usize) {
        let base = slot.va_base().as_u64() + KSTACK_GUARD_SIZE;
        for i in 0..mapped {
            let va = VirtAddr::new(base + (i as u64) * PAGE_SIZE_4KB);
            let pa: PhysAddr = unmap_page(va);
            if !pa.is_null() {
                free_page_frame(pa);
            }
        }
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        // Intentionally do NOT unmap or free frames here.  Unmapping a
        // kernel-VA page triggers a broadcast TLB shootdown; under task
        // churn that floods the IPI path.  We keep the mapping alive
        // and let the next allocation reuse this slot (see
        // `KernelStack::allocate` and `KstackVaAllocator`).  The slot
        // handle drops next, flipping the "free" bitmap bit so a future
        // `alloc_slot` can reclaim this slot.
    }
}
