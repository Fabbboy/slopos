//! Lifetime-branded handle for inspecting kernel `Task` slots from
//! tests. Replaces the ~150 `unsafe { (*task_ptr).field }` idiom that
//! `core::scheduler::{sched_tests, context_tests}` and `core::syscall::tests`
//! use today.
//!
//! ## Why a handle (not raw pointers)
//!
//! - **Lifetime safety:** the `'pool` lifetime ties the handle to the
//!   active [`KernelTestScope`] fixture. The scope coordinates with
//!   the zombie reaper, so a handle minted inside a scope cannot
//!   observe a recycled slot.
//! - **Refcount safety:** the handle embeds a [`TaskRefGuard`] which
//!   increments `task->refcnt` on construction and decrements on
//!   drop. Even if `KernelTestScope`'s reaper-quiescence guarantee is
//!   violated by future refactoring, the slot remains pinned for the
//!   handle's lifetime (`reap_zombies` requires
//!   `task_ref_count(raw) == Some(0)`).
//! - **Aliasing safety:** the handle exposes a shared `&Task` only.
//!   Mutations go through the existing `task_set_*` free functions
//!   in `task_accessors`, which take `*mut Task` and validate.
//! - **Grep ergonomics:** `h.process_id()` reads like Asterinas/Theseus
//!   `task.process_id()`. Compare to the previous
//!   `unsafe { (*p).process_id }` form.
//!
//! ## What's the surface
//!
//! - [`TaskHandle<'pool>`]: borrowed handle, `Copy`-free (carries a
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

use super::exit_info::ExitInfo;
use super::task::{
    TaskStatus, task_borrow, task_dec_ref, task_find_by_cr3, task_find_by_id, task_inc_ref,
    task_pointer_is_valid, task_waiter_count,
};
use super::task_struct::{Task, TaskPriority};
use super::test_fixture::KernelTestScope;

// =============================================================================
// TaskHandle
// =============================================================================

/// Lifetime-branded inspection handle for a kernel task slot.
///
/// `TaskHandle<'pool>` is constructed only inside an active
/// [`KernelTestScope`], with `'pool` tied to a borrow of the scope.
/// Carries a `TaskRefGuard`-equivalent reference bump so the slot
/// stays pinned for the handle's lifetime regardless of scope
/// quiescence guarantees.
pub struct TaskHandle<'pool> {
    raw: NonNull<Task>,
    _marker: PhantomData<&'pool Task>,
}

impl<'pool> TaskHandle<'pool> {
    /// Internal constructor — validates `raw` is a live pool slot and
    /// bumps the refcount. Returns `None` for null / unmapped / dead
    /// pointers.
    fn from_raw(raw: *mut Task) -> Option<Self> {
        if raw.is_null() || !task_pointer_is_valid(raw as *const Task) {
            return None;
        }
        // Bump the refcount via the existing safe-fn API. Free on Drop.
        let _ = task_inc_ref(raw);
        let nn = NonNull::new(raw)?;
        Some(Self {
            raw: nn,
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
        // task_borrow validates the raw pointer; we know it's valid by
        // construction.
        task_borrow(self.raw.as_ptr() as *const Task).expect("TaskHandle refers to a live slot")
    }

    /// Raw `*mut Task` escape hatch for the handful of test sites
    /// that need to pass the pointer through into the existing
    /// `task_set_*` mutator APIs. Prefer field-specific safe-fn
    /// wrappers where possible.
    pub fn as_mut_ptr(&self) -> *mut Task {
        self.raw.as_ptr()
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
        self.task().last_cpu
    }

    #[inline]
    pub fn slot_index(&self) -> u32 {
        self.task().slot_index
    }

    #[inline]
    pub fn kernel_stack_top(&self) -> u64 {
        self.task().kernel_stack_top
    }

    #[inline]
    pub fn ref_count(&self) -> u32 {
        self.task().ref_count()
    }

    #[inline]
    pub fn controlling_tty(&self) -> Option<TtyIndex> {
        self.task().controlling_tty
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
            last_cpu: task.last_cpu,
            kernel_stack_top: task.kernel_stack_top,
            status: task.status(),
            exit_info_set: task.exit_info.is_set(),
            context_rip: task.context.rip,
            context_rsp: task.context.rsp,
            context_rflags: task.context.rflags,
        }
    }
}

impl<'pool> Drop for TaskHandle<'pool> {
    fn drop(&mut self) {
        // Release the refcount taken in `from_raw`. The `_` discards
        // the optional bool return (true if the slot transitioned to
        // refcount zero).
        let _ = task_dec_ref(self.raw.as_ptr());
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
pub fn by_id<'pool>(_scope: &'pool KernelTestScope, id: u32) -> Option<TaskHandle<'pool>> {
    TaskHandle::from_raw(task_find_by_id(id))
}

/// Wrap an already-located raw `*mut Task` — used by tests that
/// retain the pointer from `task_fork` / `task_clone` return values.
/// Returns `None` if the pointer is null or no longer maps to a live
/// pool slot.
pub fn wrap<'pool>(_scope: &'pool KernelTestScope, raw: *mut Task) -> Option<TaskHandle<'pool>> {
    TaskHandle::from_raw(raw)
}

/// Find a task by CR3.
pub fn by_cr3<'pool>(_scope: &'pool KernelTestScope, cr3: u64) -> Option<TaskHandle<'pool>> {
    TaskHandle::from_raw(task_find_by_cr3(cr3))
}

/// Return a handle for the currently-running BSP task.
pub fn current<'pool>(_scope: &'pool KernelTestScope) -> Option<TaskHandle<'pool>> {
    let raw = slopos_arch::pcr::get_current_task_for(0) as *mut Task;
    TaskHandle::from_raw(raw)
}
