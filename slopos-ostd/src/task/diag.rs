//! A racy, lock-free, allocation-free view of the running task, for fault and
//! panic paths.
//!
//! # Why this exists rather than the ordinary accessors
//!
//! The fault handlers in `boot/` want a handful of fields about the current
//! task — its id and name for a log line, its kernel-stack bounds to bound a
//! stack-word probe so the dump cannot itself fault. Every ordinary way of
//! getting them is unavailable in that context:
//!
//! - **No lookup.** `task_find_by_id` and `task_find_by_cr3` both take the
//!   global registry cli-spinlock. A fault that arrives while any CPU holds it
//!   deadlocks the dump, so the machine hangs instead of describing why it
//!   died.
//! - **No owning handle.** Upgrading to a `KArc` means a matching drop, and
//!   that drop can win the one-to-zero transition and run the allocator-heavy
//!   destructor — from inside a fault handler, possibly on an IST stack. That
//!   is the slab/LUF deadlock, reached from the one context that cannot
//!   recover from it.
//! - **No borrow.** A `&TaskInner` asserts the task is there for the
//!   reference's whole life. A fault path cannot honestly promise that: it may
//!   have arrived *because* something is corrupt, and the task may be
//!   mid-destruction.
//!
//! `check_task_ownership.sh` check 7 enforces the first two on
//! `boot/src/exception.rs`, `boot/src/idt.rs` and every `slopos-ostd/src/panic*`
//! file.
//!
//! # What this does instead
//!
//! [`current_task_diag`] reads `pcr::current_task_id()` first. That is a
//! gs-relative load of a value `set_current_task` publishes alongside the
//! pointer, and an invalid id means the slot names no heap task — the pre-heap
//! bootstrap stub a CPU parks on, or nothing at all. So the stub is filtered
//! out *without dereferencing it*. Only then does it `read_volatile` each field
//! it needs through a raw field pointer, forming no reference and minting no
//! handle.
//!
//! # The residual hazard, stated rather than implied away
//!
//! A volatile read still touches memory that could in principle have been
//! freed. Nothing here makes that impossible. Two things make it acceptable:
//!
//! - A task that is some CPU's `PCR.current_task` cannot be reaped — the reap
//!   gate declines while `task_is_dispatch_pinned` holds, and its second
//!   disjunct is exactly "some CPU names this task as its current". So the
//!   task this reads is pinned by the very fact that made it readable.
//! - The alternative is no diagnostics at all. A fault path that refuses to
//!   read anything it cannot prove sound prints nothing, and the failure it
//!   was there to explain goes unexplained.
//!
//! Torn values are expected and accepted: the owning CPU may be writing these
//! fields concurrently, and every consumer is a log line or a probe bound. A
//! torn stack bound produces a shorter or oddly-placed dump, never a fault —
//! the probe range-checks every address it reads.

use core::ptr::addr_of;

use slopos_abi::task::{INVALID_TASK_ID, TASK_NAME_MAX_LEN};

use crate::cpu::x86_64::pcr;
use crate::task::kernel_task::TaskInner;

/// A point-in-time copy of the diagnostic fields of the running task.
///
/// By value, and small: the whole struct is well under a cache line's worth of
/// scalars plus the fixed-size name array, so it costs a fault handler nothing
/// against the 2 KiB stack-frame gate. Copying is the point — the caller ends
/// up holding data, not a pointer into a task that may be dying.
#[derive(Clone, Copy)]
pub struct TaskDiag {
    /// The id `set_current_task` published. Never `INVALID_TASK_ID`.
    pub id: u32,
    /// The raw, NUL-padded name bytes.
    pub name: [u8; TASK_NAME_MAX_LEN],
    /// Kernel-stack bounds, or `(0, 0)` if unset. Used to bound a stack probe.
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
    ///
    /// Folding the fallback in here keeps the two fault handlers from
    /// open-coding the same "bounds look sane?" test differently.
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
/// no heap task.
///
/// Takes no lock, allocates nothing, forms no reference and mints no handle.
/// See the module docs for the residual hazard this accepts and why.
///
/// `None` covers both "no task" and "a pre-heap bootstrap stub", because
/// `set_current_task` is the sole publisher of the (pointer, id) pair and
/// stamps `INVALID_TASK_ID` whenever the slot does not name a heap task.
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
    // Every access is a `read_volatile` through a raw field pointer — no
    // reference is formed, so a concurrent write by the owning CPU is a torn
    // read rather than UB, and torn is acceptable for every consumer here.
    unsafe {
        Some(TaskDiag {
            id,
            name: addr_of!((*task).name).read_volatile(),
            kernel_stack_base: addr_of!((*task).kernel_stack_base).read_volatile(),
            kernel_stack_top: addr_of!((*task).kernel_stack_top).read_volatile(),
            flags: addr_of!((*task).flags).read_volatile(),
            // `context` is a `TaskOwnCell`; `as_ptr_racy` is its sanctioned
            // unsynchronised read path and yields a pointer without forming a
            // reference, which is exactly what this needs.
            //
            // `read_unaligned`, not `read_volatile`: `TaskContext` is
            // `#[repr(C, packed)]`, so a pointer to one of its `u64` fields
            // carries no alignment guarantee and `read_volatile` would be
            // unaligned UB. The rest of the fields above live in `repr(C)`
            // `TaskInner` at natural alignment.
            context_cr3: addr_of!((*(*task).context.as_ptr_racy()).cr3).read_unaligned(),
        })
    }
}
