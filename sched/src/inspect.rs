//! Lifetime-branded handle for inspecting kernel tasks from
//! tests. Replaces the ~150 `unsafe { (*task_ptr).field }` idiom that
//! `core::scheduler::{sched_tests, context_tests}` and `core::syscall::tests`
//! use today.
//!
//! ## Why a handle (not raw pointers)
//!
//! - **Lifetime safety:** the `'scope` lifetime ties the handle to the
//!   active [`KernelTestScope`] fixture.
//! - **Ownership safety:** the handle owns a registry-upgraded `TaskRef`,
//!   pinning the task for the handle's lifetime.
//! - **Aliasing safety:** the handle exposes a shared `&Task` only.
//!   Mutations go through the existing `task_set_*` free functions
//!   in `task_accessors`, which take `*mut Task` and validate.
//! - **Grep ergonomics:** `h.process_id()` reads like Asterinas/Theseus
//!   `task.process_id()`. Compare to the previous
//!   `unsafe { (*p).process_id }` form.
//!
//! ## What's the surface
//!
//! - [`TaskHandle<'scope>`]: borrowed handle, `Copy`-free (carries a
//!   ref guard), construction via [`by_id`] / [`current`] / [`by_cr3`]
//!   that take a `&KernelTestScope`.
//! - [`TaskSnapshot`]: POD struct capturing the most-frequently-read
//!   fields in one bulk-read. Use it when comparing pre/post-yield
//!   state so multiple field reads don't interleave with a
//!   reschedule.
//!
//! ## How it lives outside the `*test*` glob
//!
//! The kernel's test-unsafe gate forbids the literal token `unsafe` in
//! any file whose basename matches `*test*`. This module is named
//! `inspect.rs` — kernel-side, but outside the glob — so callsites in
//! `sched_tests.rs`, `context_tests.rs`, and `syscall/tests.rs` import
//! a safe surface and the unsafe absorption remains in
//! `task/task_accessors.rs` and the inner `task_borrow` lookup.

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use slopos_abi::syscall::TtyIndex;
use slopos_ostd::task::task_placement_strong_count;

use super::exit_info::ExitInfo;
use super::task::{
    TaskRef, TaskStatus, task_find_by_cr3, task_find_by_id, task_id_of, task_waiter_count,
};
use super::task_struct::{Task, TaskPriority};
use super::test_fixture::KernelTestScope;

// =============================================================================
// TaskHandle
// =============================================================================

/// Lifetime-branded inspection handle for a kernel task slot.
///
/// `TaskHandle<'scope>` is constructed only inside an active
/// [`KernelTestScope`], with `'scope` tied to a borrow of the scope.
/// Carries a strong registry guard so the task stays pinned for the handle's
/// lifetime regardless of scope quiescence guarantees.
pub struct TaskHandle<'scope> {
    task: TaskRef,
    _marker: PhantomData<&'scope Task>,
}

impl<'scope> TaskHandle<'scope> {
    /// Internal constructor — validates `raw` through the weak registry and
    /// retains the upgraded strong reference.
    fn from_raw(raw: *mut Task) -> Option<Self> {
        let id = task_id_of(raw)?;
        let task = task_find_by_id(id)?;
        if task.as_ptr() != raw {
            return None;
        }
        Some(Self {
            task,
            _marker: PhantomData,
        })
    }

    /// Borrow the underlying `Task`. The borrow lifetime is tied to
    /// `self`, so multiple field reads are sound for the duration of
    /// the test body. Mutations through other free functions remain
    /// possible (the kernel scheduler is naturally multi-writer at
    /// the field level — tests should not assume otherwise without
    /// explicit synchronisation).
    pub fn task(&self) -> &Task {
        &self.task
    }

    /// Raw `*mut Task` escape hatch for the handful of test sites
    /// that need to pass the pointer through into the existing
    /// `task_set_*` mutator APIs. Prefer field-specific safe-fn
    /// wrappers where possible.
    pub fn as_mut_ptr(&self) -> *mut Task {
        self.task.as_ptr()
    }

    // -------- frequency-sorted field accessors --------

    #[inline]
    pub fn task_id(&self) -> u32 {
        self.task().task_id
    }

    #[inline]
    pub fn process_id(&self) -> u32 {
        self.task().process_id
    }

    #[inline]
    pub fn pgid(&self) -> u32 {
        self.task().pgid
    }

    #[inline]
    pub fn sid(&self) -> u32 {
        self.task().sid
    }

    #[inline]
    pub fn status(&self) -> TaskStatus {
        self.task().status()
    }

    #[inline]
    pub fn priority(&self) -> TaskPriority {
        self.task().priority
    }

    #[inline]
    pub fn flags(&self) -> u16 {
        self.task().flags
    }

    #[inline]
    pub fn last_cpu(&self) -> u8 {
        self.task().last_cpu()
    }

    #[inline]
    pub fn kernel_stack_top(&self) -> u64 {
        self.task().kernel_stack_top
    }

    /// Current strong reference count on the task, including this handle's own
    /// registry guard. Diagnostic only.
    #[inline]
    pub fn strong_count(&self) -> u32 {
        NonNull::new(self.task.as_ptr())
            .map(|node| task_placement_strong_count(node) as u32)
            .unwrap_or(0)
    }

    #[inline]
    pub fn controlling_tty(&self) -> Option<TtyIndex> {
        self.task().controlling_tty()
    }

    #[inline]
    pub fn exit_info_is_set(&self) -> bool {
        self.task().exit_info.is_set()
    }

    /// Borrow the exit-info payload, if published. Returns `None`
    /// while the cell is empty (task not yet exited).
    #[inline]
    pub fn exit_info(&self) -> Option<&ExitInfo> {
        self.task().exit_info.try_get()
    }

    #[inline]
    pub fn signal_pending(&self) -> u64 {
        self.task().signal_pending.load(Ordering::Acquire)
    }

    #[inline]
    pub fn waiter_count(&self) -> usize {
        task_waiter_count(self.task())
    }

    /// Context register reads — used by `context_tests.rs` for the
    /// pre/post-yield register-preservation checks.
    #[inline]
    pub fn context_rsp(&self) -> u64 {
        self.task().context.rsp
    }

    #[inline]
    pub fn context_rip(&self) -> u64 {
        self.task().context.rip
    }

    #[inline]
    pub fn context_rflags(&self) -> u64 {
        self.task().context.rflags
    }

    #[inline]
    pub fn context_cr3(&self) -> u64 {
        self.task().context.cr3
    }

    /// Bulk-read snapshot of the most-frequently-tested fields. Used
    /// when comparing pre/post-yield task state so field reads don't
    /// interleave with a reschedule.
    pub fn snapshot(&self) -> TaskSnapshot {
        let task = self.task();
        TaskSnapshot {
            task_id: task.task_id,
            process_id: task.process_id,
            pgid: task.pgid,
            sid: task.sid,
            priority: task.priority,
            flags: task.flags,
            last_cpu: task.last_cpu(),
            kernel_stack_top: task.kernel_stack_top,
            status: task.status(),
            exit_info_set: task.exit_info.is_set(),
            context_rip: task.context.rip,
            context_rsp: task.context.rsp,
            context_rflags: task.context.rflags,
        }
    }
}

// =============================================================================
// TaskSnapshot
// =============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub task_id: u32,
    pub process_id: u32,
    pub pgid: u32,
    pub sid: u32,
    pub priority: TaskPriority,
    pub flags: u16,
    pub last_cpu: u8,
    pub kernel_stack_top: u64,
    pub status: TaskStatus,
    pub exit_info_set: bool,
    pub context_rip: u64,
    pub context_rsp: u64,
    pub context_rflags: u64,
}

// =============================================================================
// Lookup helpers
// =============================================================================

/// Find a task by ID and return a handle valid for the scope's lifetime.
pub fn by_id<'scope>(_scope: &'scope KernelTestScope, id: u32) -> Option<TaskHandle<'scope>> {
    task_find_by_id(id).and_then(|task| TaskHandle::from_raw(task.as_ptr()))
}

/// Wrap an already-located raw `*mut Task` — used by tests that
/// retain the pointer from `task_fork` / `task_clone` return values.
/// Returns `None` if the pointer is null or no longer maps to a live task.
pub fn wrap<'scope>(_scope: &'scope KernelTestScope, raw: *mut Task) -> Option<TaskHandle<'scope>> {
    TaskHandle::from_raw(raw)
}

/// Find a task by CR3.
pub fn by_cr3<'scope>(_scope: &'scope KernelTestScope, cr3: u64) -> Option<TaskHandle<'scope>> {
    task_find_by_cr3(cr3).and_then(|task| TaskHandle::from_raw(task.as_ptr()))
}

/// Return a handle for the currently-running BSP task.
///
/// Goes through the published id rather than the PCR pointer. The pointer may
/// name a pre-heap bootstrap stub — which is what a CPU parks on before its
/// first dispatch, and what `KernelTestScope` installs on every fixture entry —
/// and a stub is eight bytes, so reading a `Task` out of one runs off the end
/// of the object.
pub fn current<'scope>(_scope: &'scope KernelTestScope) -> Option<TaskHandle<'scope>> {
    let id = slopos_arch::pcr::current_task_id_for(0);
    if id == crate::task::INVALID_TASK_ID {
        return None;
    }
    task_find_by_id(id).and_then(|task| TaskHandle::from_raw(task.as_ptr()))
}
