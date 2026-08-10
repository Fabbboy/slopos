//! Ownership of a half-built task for the length of its build.
//!
//! A [`PendingTask`] is the sole reference to a task that already owns its
//! kernel stack, its data stack and its process VM, and it is deliberately
//! *unregistered*: no lookup, no active-task walk, no census and no shutdown
//! sweep can see it. That is what makes construction sound — nothing can
//! observe the task half-built — and it is also what makes a lost token
//! unrecoverable, because there is nothing left that names the orphan.
//!
//! [`SpawnGuard`] is what stops it being lost. Every exit from the build
//! releases the child through the guard's [`Drop`]: an early `return`, a `?`,
//! a panic, or a kill — a killed builder aborts out of whatever it is blocked
//! in and returns through this frame like any other.

use slopos_ostd::cpu::preempt::PreemptGuard;

use super::Task;
use super::task_lifecycle::task_abandon;
use super::task_table::{PendingTask, TaskRef};

/// Owns a half-built task for the whole construction window.
pub struct SpawnGuard {
    child_id: u32,
    child_process_id: u32,
    /// The child's descriptor table, captured at construction.
    ///
    /// Captured rather than resolved on demand: `child_process_id` is a
    /// number, and by the time a caller asked, a failed spawn could have
    /// returned it to the allocator. The table designator cannot drift that
    /// way — it fails closed instead.
    child_table: Option<slopos_fs::fileio::FdTable>,
    /// `None` once [`commit`](Self::commit) has taken it.
    pending: Option<PendingTask>,
}

impl SpawnGuard {
    /// Take ownership of `pending` for the length of the build.
    pub fn new(mut pending: PendingTask) -> Self {
        let child_id = pending.id();
        let (child_process_id, child_table) = {
            let task = pending.as_mut();
            let table = task
                .process()
                .as_deref()
                .and_then(slopos_fs::fileio::FdTable::of);
            (task.process_id, table)
        };
        Self {
            child_id,
            child_process_id,
            child_table,
            pending: Some(pending),
        }
    }

    #[inline]
    pub fn child_id(&self) -> u32 {
        self.child_id
    }

    #[inline]
    pub fn child_process_id(&self) -> u32 {
        self.child_process_id
    }

    /// The child's descriptor table. `None` for a kernel task.
    #[inline]
    pub fn child_table(&self) -> Option<slopos_fs::fileio::FdTable> {
        self.child_table
    }

    /// Exclusive access to the child under construction.
    ///
    /// `f` may allocate and may take locks. It must not **block or yield**:
    /// the preempt guard makes `assert_switch_preempt_safe` turn a violation
    /// into a panic naming this frame. Anything whose release can deallocate —
    /// a displaced `KArc` — must be returned out of `f` and dropped by the
    /// caller, because the buddy allocator's reuse path performs synchronous
    /// cross-CPU TLB drains, which is exactly what a preempt guard forbids.
    ///
    /// `None` once the guard is spent.
    pub fn with_child<R>(&mut self, f: impl FnOnce(&mut Task) -> R) -> Option<R> {
        let _preempt = PreemptGuard::new();
        let pending = self.pending.as_mut()?;
        Some(f(pending.as_mut()))
    }

    /// Make the child reachable.
    ///
    /// `None` means the registry was full — `task_commit` abandoned the token
    /// itself — or that the guard was already spent. Either way nothing is
    /// left to release.
    pub fn commit(mut self) -> Option<TaskRef> {
        let token = self.pending.take()?;
        let _preempt = PreemptGuard::new();
        super::task_lifecycle::task_commit(token)
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Some(token) = self.pending.take() {
            task_abandon(token);
        }
    }
}
