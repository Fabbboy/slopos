//! `UnsafeStack` — RAII handle for a task's SafeStack data stack.
//!
//! One [`UnsafeStack`] per task, sitting beside the task's [`KernelStack`].
//! The SafeStack sanitizer pass (enabled via `-Zsanitizer=safestack`) moves
//! address-taken locals and dynamic allocas onto this stack at each
//! instrumented function's prologue, so a write through a corrupted data
//! pointer cannot reach the return-address / register-spill region of the
//! kernel (safe) stack.  Return addresses and callee-saved registers
//! continue to live on the kernel stack exclusively — the two are in
//! disjoint VA regions (`USTACK_VA_BASE..USTACK_VA_END` vs.
//! `KSTACK_VA_BASE..KSTACK_VA_END`) so a stray data-stack OOB cannot
//! rewrite control flow.
//!
//! Layout mirrors `KernelStack` exactly: one guard page at the bottom of
//! every slot (unmapped → overflow faults), usable pages above.  The
//! per-slot drop does **not** unmap (same TLB-shootdown-storm reason as
//! the kernel stack — see `KernelStack::drop`).  Mappings live on; a
//! future allocation of the same slot skips the frame-mapping path and
//! just re-zeros the stack contents.
//!
//! # Safety
//!
//! Same argument as [`KernelStack`] — the implementation is safe Rust.
//! Exclusive ownership is enforced by [`crate::mm::ustack_va::UstackSlot`]
//! being neither `Copy` nor `Clone`.

use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_mm::memory_layout_defs::{USTACK_GUARD_SIZE, USTACK_STRIDE};
use slopos_mm::page_alloc::{alloc_page_frames_pcp_batch, free_page_frame};
use slopos_mm::paging::{map_page_4kb, unmap_page};
use slopos_mm::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use slopos_mm::ustack_va::{UstackSlot, alloc_slot};

use crate::scheduler::stack::StackAllocError;

/// Owning handle to a SafeStack-sanitizer data stack.
///
/// Not `Copy`/`Clone` — double-free is impossible by construction.
pub struct UnsafeStack {
    slot: UstackSlot,
    size: u32,
}

impl UnsafeStack {
    /// Allocate an unsafe stack of `size` bytes.
    ///
    /// Constraints match `KernelStack::allocate`:
    /// - `size` must be nonzero,
    /// - a multiple of `PAGE_SIZE_4KB`,
    /// - small enough that `size + USTACK_GUARD_SIZE <= USTACK_STRIDE`.
    pub fn allocate(size: usize) -> Result<Self, StackAllocError> {
        if size == 0 || size % PAGE_SIZE_4KB as usize != 0 {
            return Err(StackAllocError::InvalidSize);
        }
        if (size as u64) + USTACK_GUARD_SIZE > USTACK_STRIDE {
            return Err(StackAllocError::InvalidSize);
        }

        let mut slot = alloc_slot().ok_or(StackAllocError::OutOfVirtualSpace)?;
        let base = slot.va_base().as_u64() + USTACK_GUARD_SIZE;
        let page_count = size / PAGE_SIZE_4KB as usize;

        if slot.was_backed() {
            Self::zero_stack_pages(base, page_count);
            return Ok(Self {
                slot,
                size: size as u32,
            });
        }

        let flags = (PageFlags::KERNEL_RW | PageFlags::NO_EXECUTE).bits();

        const MAX_STACK_PAGES: usize = (USTACK_STRIDE / PAGE_SIZE_4KB) as usize;
        debug_assert!(page_count <= MAX_STACK_PAGES);

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
                for j in i..page_count {
                    free_page_frame(frames[j]);
                }
                Self::cleanup_partial(&slot, i);
                return Err(StackAllocError::MappingFailed);
            }
        }

        Self::zero_stack_pages(base, page_count);
        slot.mark_backed();

        Ok(Self {
            slot,
            size: size as u32,
        })
    }

    fn zero_stack_pages(base: u64, page_count: usize) {
        let bytes = page_count * PAGE_SIZE_4KB as usize;
        unsafe {
            ptr::write_bytes(base as *mut u8, 0, bytes);
        }
    }

    /// Lowest usable address (inclusive).  Guard page sits below this.
    #[inline]
    pub fn base(&self) -> VirtAddr {
        VirtAddr::new(self.slot.va_base().as_u64() + USTACK_GUARD_SIZE)
    }

    /// One-past-the-top address (exclusive).  Stacks grow downward from
    /// here toward `base()`.
    #[inline]
    pub fn top(&self) -> VirtAddr {
        VirtAddr::new(self.base().as_u64() + self.size as u64)
    }

    /// Usable size in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.size as usize
    }

    fn cleanup_partial(slot: &UstackSlot, mapped: usize) {
        let base = slot.va_base().as_u64() + USTACK_GUARD_SIZE;
        for i in 0..mapped {
            let va = VirtAddr::new(base + (i as u64) * PAGE_SIZE_4KB);
            let pa: PhysAddr = unmap_page(va);
            if !pa.is_null() {
                free_page_frame(pa);
            }
        }
    }
}

impl Drop for UnsafeStack {
    fn drop(&mut self) {
        // Intentionally do NOT unmap — same rationale as
        // `KernelStack::drop`.  Unmapping kernel-VA pages triggers a
        // broadcast TLB shootdown IPI per page; under task churn
        // (tests that create and destroy thousands of tasks) the
        // shootdown path becomes the bottleneck and can hang CPUs.
        // The slot returns to the per-CPU cache still backed so the
        // next allocation of the same slot skips the mapping path
        // and just re-zeros the contents.  Peak physical memory =
        // peak concurrent tasks × `TASK_UNSAFE_STACK_SIZE`.
    }
}
