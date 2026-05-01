//! `UnsafeStack` — RAII handle for a task's SafeStack data stack.
//!
//! Type-aliased to the generic [`super::task_stack::TaskStack`]
//! parameterised over [`UstackRegion`].  All allocation logic, frame
//! mapping, and Drop semantics live in `task_stack.rs`; this file only
//! provides the historical name so existing callers compile unchanged.
//!
//! # SafeStack background
//!
//! One [`UnsafeStack`] per task, sitting beside the task's
//! [`super::stack::KernelStack`].  The SafeStack sanitiser pass
//! (enabled via `-Zsanitizer=safestack`) moves address-taken locals
//! and dynamic allocas onto this stack at each instrumented function's
//! prologue, so a write through a corrupted data pointer cannot reach
//! the return-address / register-spill region of the kernel (safe)
//! stack.  Return addresses and callee-saved registers continue to
//! live on the kernel stack exclusively — the two are in disjoint VA
//! regions (`USTACK_VA_BASE..USTACK_VA_END` vs.
//! `KSTACK_VA_BASE..KSTACK_VA_END`) so a stray data-stack OOB cannot
//! rewrite control flow.
//!
//! Layout mirrors `KernelStack` exactly: one guard page at the bottom
//! of every slot (unmapped → overflow faults), usable pages above.
//! Drop does **not** unmap (TLB-shootdown-storm reason — see the doc
//! on `TaskStack::<R>::drop`).  Mappings live on; a future allocation
//! of the same slot skips the frame-mapping path and just re-zeros the
//! stack contents.

use slopos_mm::stack_region::UstackRegion;

use super::task_stack::TaskStack;

/// Owning handle to a SafeStack-sanitiser data stack.
///
/// Distinct from [`super::stack::KernelStack`] at the type level
/// (different `R` parameter) — passing one where the other is expected
/// is a compile error.
pub type UnsafeStack = TaskStack<UstackRegion>;
