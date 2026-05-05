//! `TaskRuntimeBackend` impl backed by the per-CPU PCR `current_task`
//! slot.
//!
//! `slopos_ostd::task::current()` is the OSTD-side surface that mints a
//! `CurrentTask` token from the running task pointer.  Until the kernel
//! `Task` struct is re-skinned over `slopos_ostd::task::Task` (planned
//! for a later phase), the kernel writes a raw `*mut ()` into the PCR
//! slot and OSTD treats the returned pointer as opaque — no field
//! access, just a non-null check.  The cast from `*mut ()` to
//! `*const slopos_ostd::task::Task` is therefore structurally a no-op.
//!
//! This backend lives in `kernel-services` (rather than `slopos-core`)
//! to avoid a dependency cycle: `slopos-core` already depends on
//! `slopos-kernel-services`, so the inverse arrow is unavailable.
//! Routing through `slopos_arch::pcr` (which has no dep on
//! `slopos-core`) keeps the dependency graph acyclic, mirroring the
//! pattern established by `PcrPreemptBackend`.

use slopos_ostd::task::{Task, TaskRuntimeBackend};

pub struct PcrTaskRuntimeBackend;

pub static PCR_TASK_RUNTIME: PcrTaskRuntimeBackend = PcrTaskRuntimeBackend;

// SAFETY: `current_task` reads the per-CPU PCR slot via
// `slopos_arch::pcr::get_current_task`.  The slot is `AtomicPtr<()>`
// loaded with `Ordering::Acquire` and the kernel scheduler is the sole
// writer (through `dispatch()`).  When no task has been dispatched yet
// the load returns null, which the OSTD `current()` surface rejects
// with a panic — matching the contract for `TaskRuntimeBackend`.
unsafe impl TaskRuntimeBackend for PcrTaskRuntimeBackend {
    fn current_task(&self) -> *const Task {
        // Inv. 8 — the kernel scheduler ensures the slot points at the
        // task currently dispatched on this CPU and at no other CPU.
        slopos_arch::pcr::get_current_task() as *const Task
    }
}
