//! Generic RAII handle owning one task stack: VA slot + physical frames, in one
//! implementation parameterised over [`StackRegion`].
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
//! # Safety
//!
//! The implementation is **safe Rust**.  `kernel_map_4kb` and
//! `kernel_unmap_4kb` are safe wrappers in `mm::kernel_mappings`,
//! driving the kernel master's `VmSpace` cursor, so no `unsafe`
//! blocks are needed here.  Correctness comes from:
//!
//! - Exclusive ownership of the slot (via [`StackSlot<R>`], which is
//!   neither `Copy` nor `Clone`) — no two `TaskStack`s can ever name
//!   the same VA range.
//! - `VirtAddr::new` enforces canonical form.
//! - The guard page is always included by the stride arithmetic; it
//!   cannot be accidentally mapped.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::quota::KernelMetaAxis;
use slopos_mm::kernel_mappings::{kernel_map_4kb, kernel_unmap_4kb};
use slopos_mm::page_alloc::{alloc_page_frames_pcp_batch, free_page_frame};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::stack_region::{KstackRegion, StackRegion, UstackRegion};
use slopos_mm::stack_va::{self, StackSlot};
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Charge, try_charge};

/// Maximum pages in any region's slot, excluding the guard page: both kstack
/// and ustack use a 64 KB stride. `TaskStack::<R>::allocate`'s compile-time
/// assertion rejects a region whose stride would overflow this buffer.
const MAX_STACK_PAGES_PER_SLOT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackAllocError {
    /// `size` was zero, not a multiple of 4 KB, or larger than
    /// `R::STRIDE - R::GUARD_SIZE`.
    InvalidSize,
    /// No free slot remains in the `R` VA region.
    OutOfVirtualSpace,
    /// The page allocator could not satisfy a frame request.
    OutOfPhysicalFrames,
    /// `kernel_map_4kb` returned an error, typically out of memory for
    /// intermediate page tables.
    MappingFailed,
}

/// Owning handle to one mapped task stack in region `R`.
///
/// Not `Copy`/`Clone` — double-free is impossible by construction.
pub struct TaskStack<R: StackRegion> {
    /// Dropped after `TaskStack`'s own empty `Drop` body, so its
    /// return-to-cache logic runs at the right point in the destruction order.
    slot: StackSlot<R>,
    /// Number of bytes mapped (excluding the guard page).
    size: u32,
    /// The mapped pages, charged to the account that asked for the stack and
    /// refunded by this struct's `Drop`. On the pooled return-to-cache path the
    /// VA slot is recycled but the pages stay mapped, so they stay charged
    /// until the stack itself drops.
    #[expect(dead_code, reason = "held for ownership; dropping it is the refund")]
    pages_charge: Charge<KernelMetaAxis>,
}

impl<R: StackRegion> TaskStack<R> {
    /// `R`'s stride must fit the worst-case frames buffer; instantiated by
    /// referencing `_FITS` from `allocate`.
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
    /// On failure nothing leaks: partially-mapped pages are unmapped, their
    /// frames freed, and the slot handle dropped.
    pub fn allocate(size: usize, account: AccountId) -> Result<Self, StackAllocError> {
        // Forces the compile-time stride check to instantiate per region.
        let _: () = Self::_FITS;

        if size == 0 || size % PAGE_SIZE_4KB as usize != 0 {
            return Err(StackAllocError::InvalidSize);
        }
        if (size as u64) + R::GUARD_SIZE > R::STRIDE {
            return Err(StackAllocError::InvalidSize);
        }

        // Charged before any VA slot or frame is taken, so a refusal unwinds
        // nothing.
        let pages_charge = Charge::commit(
            try_charge::<KernelMetaAxis>(account, (size / PAGE_SIZE_4KB as usize) as u32)
                .map_err(|_| StackAllocError::OutOfPhysicalFrames)?,
        );

        let mut slot = stack_va::alloc_slot::<R>().ok_or(StackAllocError::OutOfVirtualSpace)?;
        let base = slot.va_base().as_u64() + R::GUARD_SIZE;
        let page_count = size / PAGE_SIZE_4KB as usize;

        if slot.was_backed() {
            // Still mapped from its last use: re-zero only, no TLB traffic.
            Self::zero_stack_pages(base, page_count);
            return Ok(Self {
                slot,
                size: size as u32,
                pages_charge,
            });
        }

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
            let map_rc = kernel_map_4kb(va, frames[i], R::PAGE_FLAGS);
            if map_rc != 0 {
                // The loop starts at `i + 1`: `kernel_map_4kb` took ownership
                // of `frames[i]` and returned it on failure.
                for j in (i + 1)..page_count {
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
            pages_charge,
        })
    }

    /// Lowest usable address (inclusive); the guard page sits below it.
    #[inline]
    pub fn base(&self) -> VirtAddr {
        VirtAddr::new(self.slot.va_base().as_u64() + R::GUARD_SIZE)
    }

    /// One-past-the-top address (exclusive); stacks grow down toward `base()`.
    #[inline]
    pub fn top(&self) -> VirtAddr {
        VirtAddr::new(self.base().as_u64() + self.size as u64)
    }

    /// Usable size in bytes (excluding the guard page).
    #[inline]
    pub fn size(&self) -> usize {
        self.size as usize
    }

    fn zero_stack_pages(base: u64, page_count: usize) {
        let bytes = page_count * PAGE_SIZE_4KB as usize;
        slopos_ostd::util::ptr_buf::zero_bytes_at_kernel_va(base, bytes);
    }

    /// Unmap the first `mapped` pages of `slot` on `allocate`'s error path; the
    /// slot's own `Drop` releases the VA. `kernel_unmap_4kb` frees the frame it
    /// reclaims, so there is nothing left here to hand back.
    fn cleanup_partial(slot: &StackSlot<R>, mapped: usize) {
        let base = slot.va_base().as_u64() + R::GUARD_SIZE;
        for i in 0..mapped {
            let va = VirtAddr::new(base + (i as u64) * PAGE_SIZE_4KB);
            let _: PhysAddr = kernel_unmap_4kb(va);
        }
    }
}

impl<R: StackRegion> Drop for TaskStack<R> {
    fn drop(&mut self) {
        // Deliberately empty: unmapping a kernel-VA page broadcasts a TLB
        // shootdown IPI per page, which floods under task churn. The mapping
        // stays installed and the slot handle, dropping next, returns it to the
        // cache for the next allocation to reuse.
    }
}

/// Owning handle to a kernel-mode task stack.
pub type KernelStack = TaskStack<KstackRegion>;

/// Owning handle to a SafeStack-sanitiser data stack, living alongside the
/// task's [`KernelStack`]. The sanitiser pass moves address-taken locals here
/// so a write through a corrupted data pointer cannot reach the return-address
/// region of the kernel stack.
pub type UnsafeStack = TaskStack<UstackRegion>;
