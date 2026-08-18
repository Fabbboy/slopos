//! A racy, lock-free, allocation-free view of the running task, for fault and
//! panic paths.
//!
//! The ordinary accessors are all unavailable there: `task_find_by_id` and
//! `task_find_by_cr3` take the global registry cli-spinlock, so a fault that
//! arrives while any CPU holds it deadlocks the dump; upgrading to a `KArc`
//! risks the matching drop running the allocator-heavy destructor from inside a
//! fault handler, possibly on an IST stack; and a `&TaskInner` would assert a
//! liveness a fault path cannot honestly promise. `check_task_ownership.sh`
//! check 7 enforces the first two on `boot/src/exception.rs`, `boot/src/idt.rs`
//! and every `slopos-ostd/src/panic*` file.
//!
//! [`current_task_diag`] reads `pcr::current_task_id()` first: an invalid id
//! means the slot names no heap task — the pre-heap bootstrap stub a CPU parks
//! on, or nothing at all — so the stub is filtered out *without dereferencing
//! it*. Only then does it `read_volatile` each field it needs through a raw
//! field pointer, forming no reference and minting no handle.
//!
//! The residual hazard is that a volatile read still touches memory that could
//! in principle have been freed. What makes it acceptable: a task that is some
//! CPU's `PCR.current_task` cannot be reaped, because the reap gate declines
//! while `task_is_dispatch_pinned` holds and its second disjunct is exactly
//! "some CPU names this task as its current".
//!
//! Torn values are expected and accepted: the owning CPU may be writing these
//! fields concurrently, and every consumer is a log line or a probe bound that
//! range-checks every address it reads.

use core::ptr::addr_of;

use slopos_abi::task::{INVALID_TASK_ID, TASK_NAME_MAX_LEN};

use crate::cpu::x86_64::pcr;
use crate::task::kernel_task::TaskInner;

/// A point-in-time copy of the diagnostic fields of the running task.
///
/// Copying is the point: the caller ends up holding data, not a pointer into a
/// task that may be dying. Kept small enough to cost a fault handler nothing
/// against the 2 KiB stack-frame gate.
#[derive(Clone, Copy)]
pub struct TaskDiag {
    /// The id `set_current_task` published. Never `INVALID_TASK_ID`.
    pub id: u32,
    /// The raw, NUL-padded name bytes.
    pub name: [u8; TASK_NAME_MAX_LEN],
    /// Kernel-stack bounds, or `(0, 0)` if unset.
    pub kernel_stack_base: u64,
    /// Exclusive upper bound of the kernel stack.
    pub kernel_stack_top: u64,
    pub flags: u16,
    /// The task's saved `context.cr3`, for comparison against live CR3.
    pub context_cr3: u64,
}

impl TaskDiag {
    /// The name as a `&str`, truncated at the first NUL.
    #[inline]
    pub fn name_str(&self) -> &str {
        crate::string::bytes_as_str(&self.name)
    }

    /// Whether `addr` lies inside the recorded kernel stack. `false` when the
    /// bounds are unset or came back torn/inverted, which is the safe answer
    /// for a probe bound.
    #[inline]
    pub fn stack_contains(&self, addr: u64) -> bool {
        self.kernel_stack_base != 0
            && self.kernel_stack_top > self.kernel_stack_base
            && addr >= self.kernel_stack_base
            && addr < self.kernel_stack_top
    }

    /// The probe range for a stack dump: the recorded kernel stack when it is
    /// usable, otherwise a narrow window around `fallback_centre`.
    #[inline]
    pub fn probe_range(
        diag: Option<&Self>,
        fallback_centre: usize,
        radius: usize,
    ) -> (usize, usize) {
        match diag {
            Some(d) if d.kernel_stack_base != 0 && d.kernel_stack_top > d.kernel_stack_base => {
                (d.kernel_stack_base as usize, d.kernel_stack_top as usize)
            }
            _ => (
                fallback_centre.saturating_sub(radius),
                fallback_centre.saturating_add(radius),
            ),
        }
    }
}

/// Snapshot the running task's diagnostic fields, or `None` when this CPU has
/// no heap task — which covers a pre-heap bootstrap stub, since
/// `set_current_task` stamps `INVALID_TASK_ID` for one.
///
/// Takes no lock, allocates nothing, forms no reference and mints no handle.
/// See the module docs for the residual hazard this accepts and why.
#[inline]
pub fn current_task_diag<K, U>() -> Option<TaskDiag>
where
    TaskInner<K, U>: crate::task::PcrTaskType,
{
    let id = pcr::current_task_id();
    if id == INVALID_TASK_ID {
        return None;
    }
    let task = pcr::get_current_task().cast::<TaskInner<K, U>>();
    if task.is_null() {
        return None;
    }

    // SAFETY: the id check above establishes the slot names a heap task, and a
    // task that is this CPU's current cannot be reaped (see the module docs).
    // Every access reads through a raw field pointer, so a concurrent write by
    // the owning CPU is a torn read rather than UB.
    unsafe {
        Some(TaskDiag {
            id,
            name: addr_of!((*task).name).read_volatile(),
            kernel_stack_base: addr_of!((*task).kernel_stack_base).read_volatile(),
            kernel_stack_top: addr_of!((*task).kernel_stack_top).read_volatile(),
            flags: addr_of!((*task).flags).read_volatile(),
            // `as_ptr_racy` is the `TaskOwnCell`'s sanctioned unsynchronised
            // read path, and yields a pointer without forming a reference.
            // `read_unaligned`, not `read_volatile`: `TaskContext` is
            // `#[repr(C, packed)]`, so a pointer to one of its `u64` fields
            // carries no alignment guarantee.
            context_cr3: addr_of!((*(*task).context.as_ptr_racy()).cr3).read_unaligned(),
        })
    }
}
