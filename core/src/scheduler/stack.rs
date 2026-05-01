//! `KernelStack` — RAII handle for a task's kernel-mode stack.
//!
//! Type-aliased to the generic [`super::task_stack::TaskStack`]
//! parameterised over [`KstackRegion`].  All allocation logic, frame
//! mapping, and Drop semantics live in `task_stack.rs`; this file only
//! provides the historical name and the shared `StackAllocError` enum
//! so existing callers compile unchanged.
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

use slopos_mm::stack_region::KstackRegion;

use super::task_stack::TaskStack;

/// Owning handle to a kernel-mode task stack.
///
/// Distinct from [`super::unsafe_stack::UnsafeStack`] at the type level
/// (different `R` parameter) — passing one where the other is expected
/// is a compile error.  Not `Copy`/`Clone` — double-free is impossible
/// by construction.
pub type KernelStack = TaskStack<KstackRegion>;

/// Reasons `KernelStack::allocate` / `UnsafeStack::allocate` can fail.
///
/// Canonical home for the shared error type.  `task_stack` re-exports
/// from here.  Phase 6 of the unification will fold this definition
/// into `task_stack.rs` when this module is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackAllocError {
    /// `size` was zero, not a multiple of 4 KB, or larger than the
    /// region's slot stride minus the guard page.
    InvalidSize,
    /// No free slot remains in the region's VA range.
    OutOfVirtualSpace,
    /// The page allocator could not satisfy a frame request.
    OutOfPhysicalFrames,
    /// `map_page_4kb` returned an error (typically out of memory for
    /// intermediate page tables).
    MappingFailed,
}
